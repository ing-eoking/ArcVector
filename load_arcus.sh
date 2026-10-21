#!/bin/sh
# Load a parts directory into a running arcus.
#
#   ./tools/load_arcus.sh <parts-dir> <host> <port>
#   ./tools/load_arcus.sh data/dataset-rag localhost 11399
#
# Reads chunks.tsv / vectors.txt / manifest.sh from the directory, sends vadd
# for the vector and set for the original row under the same id, printing a
# progress counter on stdout as it goes. No files are written.
#
# Line N of vectors.txt and line N of chunks.tsv are the same record, so awk
# walks chunks.tsv and pulls the coordinate line in with getline. awk builds the
# whole protocol stream: a shell read loop spends more time crossing process
# boundaries than the server spends storing.
#
# It used to be `paste chunks.tsv vectors.txt | awk`, and on a 1024-dim corpus
# paste cost more than the server did: it runs at ~3k rows/s on a full core, so
# four shards spent four cores rebuilding the same 10GB stream. getline inside
# the same awk is 2.7x faster and leaves the cores for the connections.
#
# KV values are "title<TAB>abstract" - both fields are tab- and newline-free by
# construction, so they need no escaping and split on the first tab.
#
# Environment:
#   KV=0            vectors only, skip the KV copy
#   KEY_PREFIX=...  KV key prefix (default "<index>:", so arxiv_rag:k42)
#   EXPTIME=n       TTL on the vectors and the KV items (default 0, no expiry)
#   LIMIT=n         stop after n rows
#   JOBS=n          n connections in parallel (default 1)
#
# JOBS splits the rows round-robin -- shard k takes every nth line starting at
# k -- so no temporary files are needed and the promise above holds. Each shard
# reads the two input files itself, so the file reads are n times over, and that
# is the price of the shards being independent: one reader dealing rows over n
# connections reads the files once, but a round-robin write blocks on whichever
# connection is busiest while the other n-1 sit idle waiting their turn, which
# measured four times slower than n independent shards. Each shard owns two
# status lines of its own, and its rate and eta describe that shard, not the
# whole load.
#
# The awk variable holding EXPTIME is named ttl, never exp: exp() is an awk
# builtin, so -v exp=0 never binds a variable and the exptime field silently
# receives exp(0) = 1, putting a one-second TTL on every item.
set -eu

dir=${1:?usage: load_arcus.sh <parts-dir> <host> <port>}
host=${2:-localhost}
port=${3:-11211}
. "$dir/manifest.sh"

KV=${KV:-1}
KEY_PREFIX=${KEY_PREFIX:-"$INDEX:"}
EXPTIME=${EXPTIME:-0}
LIMIT=${LIMIT:-0}
JOBS=${JOBS:-1}

[ "$JOBS" -ge 1 ] || { echo "JOBS must be at least 1" >&2; exit 1; }

nc -z "$host" "$port" 2>/dev/null || {
    echo "nothing listening on $host:$port" >&2
    exit 1
}

ping_reply=$(printf "set arcus:zk-ping 1 0 1\r\n1\r\n" | nc -w 1 "$host" "$port" | tr -d '\r')
if [ "$ping_reply" != "STORED" ]; then
    echo "Target node $host:$port is not writable (reply: $ping_reply). Aborting load." >&2
    exit 1
fi

# awk writes the protocol on its stdout, which is the pipe into nc, so progress
# needs a channel of its own: fd 3 keeps pointing at the script's stdout
exec 3>&1

rows=$COUNT
[ "$LIMIT" -gt 0 ] && [ "$LIMIT" -lt "$COUNT" ] && rows=$LIMIT
start=$(date +%s)

# redraw every row on a terminal; when the output is a pipe or a file there is
# nothing to redraw, so fall back to an occasional line.
# The test has to happen here, not inside $(...): a command substitution runs
# with its stdout on a pipe, so [ -t 1 ] there is always false and the status
# line would scroll away instead of redrawing. TTY=1/0 overrides the guess.
if [ -t 1 ]; then
    tty=${TTY:-1}
else
    tty=${TTY:-0}
fi
# a redraw per row is a thousand escape sequences a second, which the terminal
# spends more time on than the load does; a few per second reads the same
if [ "$tty" = 1 ]; then tick=${TICK:-200}; else tick=${TICK:-5000}; fi

# vcreate no longer takes MAXCOUNT or EXPTIME -- the graph grows on its own and
# the expiry moved to each vadd -- but manifests written before that still carry
# them, and the server refuses the whole line over an unknown option. Options
# come in KEY VALUE pairs after `vcreate <index> <dim>`, so drop those two pairs
# and keep the rest as written.
vcreate=$(printf '%s\n' "$VCREATE" | awk '{
    out = $1 " " $2 " " $3
    for (i = 4; i < NF; i += 2) {
        k = toupper($i)
        if (k != "MAXCOUNT" && k != "EXPTIME") out = out " " $i " " $(i + 1)
    }
    print out
}')

# The index is created once, on a connection of its own, before any shard opens.
# Shards must not race it into existence: the loser would see EXISTS, which the
# reply reader cannot tell from an index that was already there, and rows would
# go out while the create was still in flight.
create_reply=$(printf '%s\r\nquit\r\n' "$vcreate" | nc -w 5 "$host" "$port" | tr -d '\r' | head -1)
case $create_reply in
    CREATED|EXISTS) ;;
    *) echo "vcreate refused (reply: $create_reply). Aborting load." >&2; exit 1 ;;
esac

# reserve the two lines each shard owns, then sit on the first of them. No
# writer may emit a newline afterwards: a newline scrolls the block, and every
# other writer then draws over the line it just moved off.
lines=$(( JOBS * 2 ))
[ "$tty" = 1 ] && { i=0; while [ "$i" -lt "$lines" ]; do printf '\n'; i=$(( i + 1 )); done; printf '\033[%dA' "$lines"; }

# Ctrl-C leaves two messes to clean up. The terminal driver signals the whole
# foreground group, so most of the pipeline dies on its own, but whatever
# handles or blocks SIGINT does not -- so the group gets a TERM as well. And the
# cursor is parked inside the reserved block with no newline under it, where the
# shell prompt would draw over a status line; stepping past the block first
# leaves the last drawn state readable.
#
# The trap is disarmed before that kill: `kill 0` reaches this process too, and
# a trap that re-enters itself never finishes.
interrupted() {
    trap '' INT TERM
    [ "$tty" = 1 ] && printf '\033[%dB\n' "$lines"
    kill -TERM 0 2>/dev/null
    printf '  interrupted after %ds -- the load is partial\n' "$(( $(date +%s) - start ))" >&2
    exit 130
}
trap interrupted INT TERM

shard=0
while [ "$shard" -lt "$JOBS" ]; do
# Shard k takes rows k, k+JOBS, k+2*JOBS ... so its share is rows/JOBS rounded
# up for the first rows%JOBS shards. The status line needs that number, not the
# total, or every shard would report itself as 1/JOBS done at the end.
share=$(( rows / JOBS ))
[ "$shard" -lt $(( rows % JOBS )) ] && share=$(( share + 1 ))
(
{
    LC_ALL=C awk -F'\t' \
        -v idx="$INDEX" -v kv="$KV" -v pfx="$KEY_PREFIX" -v ttl="$EXPTIME" \
        -v lim="$LIMIT" -v total="$share" -v tick="$tick" -v tty="$tty" \
        -v shard="$shard" -v jobs="$JOBS" -v vf="$dir/$VECTORS" '
        # macOS awk has no systime(), so the clock costs a process; call it
        # every 1000 rows and carry the rate between calls
        function clock(   now) { "date +%s" | getline now; close("date +%s"); return now + 0 }
        # this shards status line sits 2*shard rows below the anchor; the moves
        # travel down and back in one write so the cursor is where every other
        # writer expects it again before the next one runs
        # tag is built here, not where it is printed: inside printf awk reads a
        # bare > as an output redirection, so `jobs > 1 ? ...` there parses as a
        # redirect to the ternary and is a syntax error
        BEGIN {
            t0 = clock()
            tag = (jobs > 1) ? sprintf("#%d ", shard) : ""
            if (shard > 0) { down = sprintf("\033[%dB", 2 * shard); up = sprintf("\033[%dA", 2 * shard) }
        }
        lim > 0 && NR > lim { exit }
        {
            # the coordinate line has to be pulled for every row, before the
            # shard filter: it is what keeps this file in step with the other
            if ((getline vec < vf) <= 0) { fail = sprintf("vectors.txt ran out at row %d", NR); exit }
            if ((NR - 1) % jobs != shard) next
            # 1 id  2 veclen  3 dim  4 attrlen  5 attr ... 9 title  10 text
            # vadd <index> <id> <veclen> <exptime> [ATTR <json>]: the dimension
            # comes from the index and the attr carries no length -- the JSON is
            # one space-free token, and an empty one leaves the clause out
            if ($5 != "") printf "vadd %s %s %s %s ATTR %s\r\n%s\r\n", idx, $1, $2, ttl, $5, vec
            else printf "vadd %s %s %s %s\r\n%s\r\n", idx, $1, $2, ttl, vec
            if (kv) {
                # LC_ALL=C makes length() count bytes, which is what set needs:
                # a UTF-8 abstract holds more bytes than characters.
                # noreply stops the server answering these; vadd has no such
                # option, so its replies still come back and are discarded
                value = $9 "\t" $10
                printf "set %s%s 0 %s %d noreply\r\n%s\r\n", pfx, $1, ttl, length(value), value
            }
            # count sent rows, not NR: the exit rule fires on the record after
            # the limit, so NR would report one too many
            sent++
            if (sent % 1000 == 0 || sent == 1) {
                elapsed = clock() - t0
                if (elapsed > 0) {
                    rate = sent / elapsed
                    left = (total - sent) / rate
                    eta = sprintf("%d:%02d", left / 60, left % 60)
                }
            }
            if (sent % tick == 0 || sent == total) {
                line = sprintf("  %s%5.1f%%  %d/%d  %-14s %6d/s  eta %s",
                               tag, 100 * sent / total, sent, total, $1, rate, eta)
                if (tty) printf "%s\r\033[K%s%s", down, line, up > "/dev/fd/3"
                else printf "%s\n", line > "/dev/fd/3"
                fflush("/dev/fd/3")
            }
        }
        END {
            # never a newline: it would scroll the block out from under every
            # other shard. The caller steps past the whole block at the end.
            #
            # A shard can be handed nothing -- LIMIT below JOBS leaves the last
            # ones with no rows -- and the percentage would divide by zero.
            if (tty && total > 0)
                printf "%s\r\033[K  %s%5.1f%%  %d/%d  done%s",
                    down, tag, 100 * sent / total, sent, total, up > "/dev/fd/3"
            else if (tty)
                printf "%s\r\033[K  %sno rows%s", down, tag, up > "/dev/fd/3"
            if (fail != "") printf "  %s\n", fail > "/dev/stderr"
        }' "$dir/$CHUNKS"
    printf 'quit\r\n'
    sleep 2   # BSD nc closes as soon as stdin ends; let the last replies drain
} | nc "$host" "$port" | LC_ALL=C awk -v tty="$tty" -v tick="$tick" \
        -v shard="$shard" -v jobs="$JOBS" '
    # The sender owns the first line of the pair and this owns the second: write
    # below, then come back up so the cursor is where every writer expects it.
    # Two processes drawing one line each is the only way to show both the key
    # being sent and the reply coming back, since neither sees the other half.
    # With JOBS>1 the pairs stack, so this line is 2*shard+1 below the anchor.
    BEGIN { off = 2 * shard + 1; tag = (jobs > 1) ? sprintf("#%d ", shard) : "" }
    function show(text) {
        if (tty) printf "\033[%dB\r\033[K  %sarcus: %s\033[%dA\r", off, tag, text, off > "/dev/fd/3"
        else printf "  %sarcus: %s\n", tag, text > "/dev/fd/3"
        fflush("/dev/fd/3")
    }
    { sub(/\r$/, "") }
    $0 == "" { next }
    {
        n++
        ok = ($0 == "STORED" || $0 == "CREATED" || $0 == "EXISTS")
        if (!ok) { bad++; last_error = $0 }
        # every reply would mean a million redraws for no new information, so
        # follow the same tick as the status line; failures always draw
        if (!ok || n % tick == 0) show(sprintf("%s   ok %d  err %d%s",
            $0, n - bad, bad, (bad ? "  last: " last_error : "")))
    }
    END { if (n) show(sprintf("%d replies, %d error(s)", n, bad + 0)) }'
) &
    shard=$(( shard + 1 ))
done

wait
trap - INT TERM

exec 3>&-
# step past the whole block so the shell prompt lands below it
[ "$tty" = 1 ] && printf '\033[%dB\n' "$lines"
printf "  done in %ds\n" "$(( $(date +%s) - start ))"
