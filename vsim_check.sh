#!/bin/sh
# Query a loaded index with the vectors it was loaded from, and score the answers.
#
#   ./tools/vsim_check.sh <parts-dir> <host> <port>
#   ./tools/vsim_check.sh data/dataset-rag localhost 11399
#
# Line N of vectors.txt is the vector stored under line N of chunks.tsv, so
# asking for that vector's nearest neighbours should put its own key first. A
# reply whose first hit is that key is a hit; anything else is a miss. That is
# the whole check -- recall@1 against the training set, which an index that is
# built and readable should get right nearly always.
#
# The vectors go out as fast as the socket takes them and the replies are read
# by a second process, the way load_arcus.sh sends. Neither side sees the other,
# so the reader cannot be told what to expect -- it reads chunks.tsv itself with
# the same shard filter, and the Nth reply on a connection lines up with the Nth
# key it reads, because TCP and the server both keep order.
#
# One awk per shard walks chunks.tsv and pulls the matching coordinate line in
# with getline. It used to be `paste chunks.tsv vectors.txt | awk`, and on a
# 1024-dim corpus paste cost more than the server did: it runs at ~3k rows/s on
# a full core, so four shards spent four cores rebuilding the same 10GB stream.
# getline inside the same awk is 2.7x faster and leaves the cores for the
# connections -- four shards went from 2573 q/s at 671% CPU to 6420 at 392%.
#
# Environment:
#   K=n             neighbours to ask for (default 100)
#   JOBS=n          n connections in parallel (default 1)
#   LOOPS=n         passes over the file; 0 keeps going until Ctrl-C (default 0)
#   LIMIT=n         stop each pass after n rows
#   FILTER="..."    FILTER terms, space separated, ANDed (default none), e.g.
#                   FILTER="cat=cs.LG yr>=2015". Operators are = != < <= > >=,
#                   and a term is one token because the protocol has no way to
#                   quote a space inside one.
#
# A pass is one connection. LOOPS>1 reconnects each time, which is nothing
# against a full pass and lets the counters start clean; they are per pass, not
# cumulative, and every pass asks the same questions, so the hit rate is the
# number to watch and it should not move between passes.
#
# JOBS splits the rows round-robin, so shard k takes every nth line starting at
# k. Each shard reads the two input files itself -- twice over, once to send and
# once to score -- so the file reads are 2n times, and that is the price of the
# shards being independent. One reader dealing rows over n connections reads the
# files once, but a round-robin write blocks on whichever connection is busiest
# and the other n-1 sit idle waiting their turn: measured against a server with
# ordinary reply jitter, one reader over four connections managed 266 q/s where
# four independent shards managed 1070.
#
# FILTER turns the scoring off, and says so instead of printing a number that
# does not mean what it reads as. The check asks for a row's own vector back and
# a filter the row does not satisfy takes that row out of the candidate set, so
# recall@1 would count a correct answer as a miss. What the shards report under
# a filter is what the run is for -- replies, and how many of them came back
# with nothing. An empty reply is a search that ended early, and a filter narrow
# enough to produce many of them makes the q/s figure flattering rather than
# useful, so the count is on screen next to it.
set -eu

dir=${1:?usage: vsim_check.sh <parts-dir> <host> <port>}
host=${2:-localhost}
port=${3:-11211}
. "$dir/manifest.sh"

K=${K:-100}
JOBS=${JOBS:-1}
LOOPS=${LOOPS:-0}
LIMIT=${LIMIT:-0}
FILTER=${FILTER:-}

[ "$JOBS" -ge 1 ] || { echo "JOBS must be at least 1" >&2; exit 1; }
[ "$K" -ge 1 ] || { echo "K must be at least 1" >&2; exit 1; }

# The wire form is `FILTER <count> <term>...`, and the count is simply how many
# terms there are. The positional parameters are the shell's word splitter and
# its counter both, and $1..$3 were consumed into dir/host/port above.
filt=
filtered=0
if [ -n "$FILTER" ]; then
    set -- $FILTER
    filt=" FILTER $# $*"
    filtered=1
fi

nc -z "$host" "$port" 2>/dev/null || {
    echo "nothing listening on $host:$port" >&2
    exit 1
}

# One query by hand before the flood: a wrong index name, a dimension that does
# not match, an index still rebuilding or a filter term the parser rejects all
# answer here, where the reply is still readable, instead of scrolling past
# inside a status line.
first_key=$(LC_ALL=C awk -F'\t' 'NR==1 {print $1; exit}' "$dir/$CHUNKS")
probe=$(printf 'vsearch key %s %s 1%s\r\nquit\r\n' "$INDEX" "$first_key" "$filt" |
        nc -w 5 "$host" "$port" | tr -d '\r' | head -2 | tr '\n' ' ')
case $probe in
    VECTORS*|PARTIAL_VECTORS*) ;;
    *) echo "the index will not answer (reply: $probe). Aborting." >&2; exit 1 ;;
esac

exec 3>&1

rows=$COUNT
[ "$LIMIT" -gt 0 ] && [ "$LIMIT" -lt "$COUNT" ] && rows=$LIMIT
start=$(date +%s)

if [ -t 1 ]; then tty=${TTY:-1}; else tty=${TTY:-0}; fi
if [ "$tty" = 1 ]; then tick=${TICK:-200}; else tick=${TICK:-5000}; fi

lines=$(( JOBS * 2 ))
[ "$tty" = 1 ] && { i=0; while [ "$i" -lt "$lines" ]; do printf '\n'; i=$(( i + 1 )); done; printf '\033[%dA' "$lines"; }

# Same reasoning as load_arcus.sh: the terminal signals the whole group, but
# whatever handles or blocks SIGINT does not die of it, so the group gets a TERM
# too; and the cursor has to step past the block or the prompt draws over a
# status line. The trap is disarmed first because `kill 0` reaches this process.
interrupted() {
    trap '' INT TERM
    [ "$tty" = 1 ] && printf '\033[%dB\n' "$lines"
    kill -TERM 0 2>/dev/null
    printf '  stopped after %ds\n' "$(( $(date +%s) - start ))" >&2
    exit 130
}
trap interrupted INT TERM

shard=0
while [ "$shard" -lt "$JOBS" ]; do
share=$(( rows / JOBS ))
[ "$shard" -lt $(( rows % JOBS )) ] && share=$(( share + 1 ))
(
    pass=1
    while [ "$LOOPS" -eq 0 ] || [ "$pass" -le "$LOOPS" ]; do
    {
        LC_ALL=C awk -F'\t' \
            -v idx="$INDEX" -v k="$K" -v lim="$LIMIT" -v total="$share" \
            -v tick="$tick" -v tty="$tty" -v shard="$shard" -v jobs="$JOBS" \
            -v pass="$pass" -v vf="$dir/$VECTORS" -v filt="$filt" '
            function clock(   now) { "date +%s" | getline now; close("date +%s"); return now + 0 }
            # built here, not where it is printed: inside printf awk reads a bare
            # > as a redirection, so `jobs > 1 ? ...` there is a syntax error
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
                # vsearch vector <index> <num> <veclen> [FILTER ...]: the index
                # knows its dimension, so only the body length goes out -- the
                # vector text as it stands, which LC_ALL=C counts in bytes.
                # filt is empty or " FILTER n term...", already assembled
                printf "vsearch vector %s %d %d%s\r\n%s\r\n", idx, k, length(vec), filt, vec
                sent++
                if (sent % 1000 == 0 || sent == 1) {
                    elapsed = clock() - t0
                    if (elapsed > 0) { rate = sent / elapsed; left = (total - sent) / rate
                                       eta = sprintf("%d:%02d", left / 60, left % 60) }
                }
                if (sent % tick == 0 || sent == total) {
                    line = sprintf("  %spass %d  %5.1f%%  %d/%d  %6d q/s  eta %s",
                                   tag, pass, 100 * sent / total, sent, total, rate, eta)
                    if (tty) printf "%s\r\033[K%s%s", down, line, up > "/dev/fd/3"
                    else printf "%s\n", line > "/dev/fd/3"
                    fflush("/dev/fd/3")
                }
            }
            END {
                if (tty && total > 0)
                    printf "%s\r\033[K  %spass %d  %5.1f%%  %d/%d  sent%s",
                        down, tag, pass, 100 * sent / total, sent, total, up > "/dev/fd/3"
                else if (tty)
                    printf "%s\r\033[K  %sno rows%s", down, tag, up > "/dev/fd/3"
                if (fail != "") printf "  %s\n", fail > "/dev/stderr"
            }' "$dir/$CHUNKS"
        printf 'quit\r\n'
        sleep 2   # BSD nc closes as soon as stdin ends; let the last replies drain
    } | nc "$host" "$port" | LC_ALL=C awk -v tty="$tty" -v tick="$tick" \
            -v shard="$shard" -v jobs="$JOBS" -v lim="$LIMIT" -v pass="$pass" \
            -v chunks="$dir/$CHUNKS" -v filtered="$filtered" '
        # The key this reply should have named. Read straight from the file with
        # the same filter the sender used, so the two streams stay in step; awk
        # keeps its own position in the file between calls. Only column 1 is
        # wanted, so the line is cut at the first tab, not split into ten fields.
        # Under a FILTER there is nothing to compare against, and this is never
        # called -- which also spares the shard a second pass over chunks.tsv.
        function expected(   line, t) {
            while ((getline line < chunks) > 0) {
                read_n++
                if (lim > 0 && read_n > lim) return ""
                if ((read_n - 1) % jobs == shard) {
                    t = index(line, "\t")
                    return t ? substr(line, 1, t - 1) : line
                }
            }
            return ""
        }
        BEGIN {
            off = 2 * shard + 1
            tag = (jobs > 1) ? sprintf("#%d ", shard) : ""
        }
        function show(text) {
            if (tty) printf "\033[%dB\r\033[K  %s%s\033[%dA\r", off, tag, text, off > "/dev/fd/3"
            else printf "  %s%s\n", tag, text > "/dev/fd/3"
            fflush("/dev/fd/3")
        }
        function report() {
            if (filtered)
                return sprintf("pass %d  replies %d  empty %d  errors %d%s", pass,
                               n, empty + 0, err + 0,
                               (part ? sprintf("  partial %d", part) : ""))
            return sprintf("pass %d  recall@1 %5.1f%%  ok %d  miss %d%s%s", pass,
                           100 * hit / n, hit, miss,
                           (part ? sprintf("  partial %d", part) : ""),
                           (miss ? "  last: " last : ""))
        }
        function score(   want) {
            n++
            if (partial) part++
            # a filtered query may legitimately exclude the row it came from, so
            # there is no expected key to compare against; count what the run is
            # actually for instead
            if (filtered) {
                if (vals == 0) empty++
            } else {
                want = expected()
                if (got == want) hit++
                else { miss++; last = sprintf("%s got %s", want, (got == "" ? "nothing" : got)) }
            }
            if (n % tick == 0) show(report())
        }
        { sub(/\r$/, "") }
        $0 == "" { next }
        # a unit is VECTORS <n> ... <id> <score> [attr] ... END, and the first
        # hit line is the answer. A hit line has no keyword of its own, so END is
        # checked before it and everything else inside a unit is a hit
        /^VECTORS / || /^PARTIAL_VECTORS / { unit = 1; got = ""; vals = 0; partial = ($0 ~ /^PARTIAL_VECTORS/); next }
        unit && $0 == "END" { unit = 0; score(); next }
        unit { vals++; if (got == "") got = $1; next }
        # anything outside a unit is the server refusing the query; it still
        # answers one request, so the expected stream has to move on with it
        {
            err++
            got = "!" $0
            vals = 0
            partial = 0
            score()
        }
        END {
            if (n) show(report() ((!filtered && err) ? sprintf("  errors %d", err) : "") "  done")
        }'
    pass=$(( pass + 1 ))
    done
) &
    shard=$(( shard + 1 ))
done

wait
trap - INT TERM

exec 3>&-
[ "$tty" = 1 ] && printf '\033[%dB\n' "$lines"
printf "  done in %ds\n" "$(( $(date +%s) - start ))"

