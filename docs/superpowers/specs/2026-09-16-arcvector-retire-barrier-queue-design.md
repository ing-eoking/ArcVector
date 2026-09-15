# 회수를 큐 순서로: 배리어가 언제 놓아도 되는지를 말한다

Design record. Status: **proposed**.

[2026-09-14-arcvector-item-trigger-design.md](2026-09-14-arcvector-item-trigger-design.md)의
§6(잠금 순서)를 이어받는다. 그 설계는 "`release`는 sweeper가 미뤄서 한다"까지 정했고,
이 설계는 **sweeper가 언제 그것을 해도 되는지**를 정한다.

---

## 1. 문제

검색은 그래프가 들고 있는 아이템 포인터를 **락 없이** 따라간다
([kv.rs:162](../../../src/handler/arcus/engine/kv.rs)의 `with_item_at`). 그것이 안전한
이유는 link 훅에게서 받은 refcount 하나뿐이다. 그러므로 **그 참조를 엔진에 돌려주는 순간이
검색과 겹치면 use-after-free**다.

지금은 epoch 기반 정지로 막는다.

| 장치 | 위치 | 하는 일 |
|---|---|---|
| 리더 슬롯 128개 | [index.rs:412](../../../src/handler/usearch/index.rs) `readers` | 검색이 슬롯을 잡고 시각을 찍는다 |
| `REGISTERING` 2단계 클레임 | [index.rs:94](../../../src/handler/usearch/index.rs) | 슬롯 확보와 시각 기록 사이의 창을 막는다 |
| `unslotted` | [index.rs:414](../../../src/handler/usearch/index.rs) | 128개가 다 찼을 때의 폴백. 0을 반환시켜 **모든 회수를 멈춘다** |
| `HOME` thread_local | [index.rs:88](../../../src/handler/usearch/index.rs) | 슬롯 탐색 시작점 분산 |
| 전역 시계 `COARSE` | [server.rs:29](../../../src/server.rs) | 리더 시각과 retire 시각의 공통 눈금 |
| `advance_epoch()` | [server.rs:52](../../../src/server.rs) | retire가 스스로 시계를 민다 |
| `retired: Vec<(u64, u64)>` | [index.rs:415](../../../src/handler/usearch/index.rs) | 주소와 그 retire 시각 |
| `oldest_reader()` / `take_ready()` | [index.rs:820](../../../src/handler/usearch/index.rs) | 유예가 끝난 것만 골라낸다 |

비싼 것은 런타임 비용이 아니라 **정확성을 증명하는 비용**이다. 최근 네 커밋이 전부 이 창들이었다.

```
29f416e  fix: claim the reader slot before reading the clock
e31a157  fix: let the retirement move the epoch, not the sweeper's tick
fed6ea1  fix: never release an item reference from the thread that retired it
fe3df78  fix: drop hits deleted while the search was running
```

그리고 이 기계는 **검색이 끊이지 않으면 한 발도 못 뗀다.** 리더가 항상 하나는 있으면
`oldest_reader()`가 전진하지 않는다.

## 2. 착상 — FIFO 큐가 순서를 이미 안다

시각이 필요했던 이유는 "이 주소가 retire된 시점과 저 검색이 시작한 시점 중 어느 쪽이
먼저인가"를 물어야 했기 때문이다. 그런데 **둘을 같은 큐에 넣으면 그 답이 큐 순서 그 자체**다.

큐에는 두 종류가 들어간다.

```
[ Barrier(n) | Release([a,b]) | Barrier(m) | Release([c]) | ... ]
  └ 검색 n개가 이 지점 이전에 시작했다      └ 놓아줄 주소
```

읽는 법은 하나다.

> `Release(X)`보다 **앞**에 있는 배리어 = X가 `held`에서 빠지기 전에 시작한 검색.
> X를 들고 있을 수 있으므로 **막아야 한다.**
>
> `Release(X)`보다 **뒤**에 있는 배리어 = X가 빠진 뒤에 시작한 검색.
> `resolve()`의 `held.contains()`가 걸러내므로 X에 닿지 못한다. **막을 이유가 없다.**

그러므로 sweeper는 큐를 앞에서부터 걸으며 **안 풀린 배리어에서 서기만** 하면 된다.
`Release`에 도달했다는 것은 그 앞의 배리어가 전부 풀렸다는 뜻이고, 그것이 곧 "이 주소를
들고 있을 수 있는 검색이 하나도 없다"는 뜻이다. 비교할 시각이 없다.

### 2.1 진행이 보장된다

이 구조가 §1의 마지막 문제를 없앤다.

```
Release(X)를 막는 것은 그 앞의 유한한 배리어들뿐이다.
나중에 시작한 검색은 전부 Release(X) 뒤에 줄을 서므로 X를 막지 못한다.
∴ 검색이 아무리 끊이지 않아도 X는 반드시 풀린다.
```

전역 "리더 0" 관측 방식은 새 검색이 계속 오면 0을 영영 못 찍어 강제 차단 장치가 필요했다.
여기서는 진행이 구조에서 나온다. 차단(§6)은 **메모리를 묶는 용도로만** 남는다.

## 3. 불변식 — 이 설계의 전부

**주소를 `held`에서 뺀 다음에 큐에 넣는다.** §2의 두 번째 문단이 여기 얹히므로 경로를
전수 확인한다.

| 경로 | 순서 | |
|---|---|---|
| `forget()` [:686](../../../src/handler/usearch/index.rs) | `give_up` → `drop(held)` → `unlink_node` → `retire` | ✓ |
| `update_published()` [:743](../../../src/handler/usearch/index.rs) | `give_up` → `drop(held)` → `retire` | ✓ |
| `remove_published()` [:1070](../../../src/handler/usearch/index.rs) | `give_up` → `drop(held)` → `retire` | ✓ |
| `forget_unreadable()` [:775](../../../src/handler/usearch/index.rs) | `give_up` → `drop(held)` → `unlink_node` → `retire` | ✓ |
| `drop_displaced()` [:704](../../../src/handler/usearch/index.rs) | 호출 전에 `give_up` [:628](../../../src/handler/usearch/index.rs) | ✓ |
| `clear_with()` [:1037](../../../src/handler/usearch/index.rs) | `take_all` → `drop(held)` → `retire` | ✓ |

### 3.1 큐를 거치지 않는 `release`는 셋뿐이고, 둘은 정당하다

| 위치 | 판정 |
|---|---|
| `Drop for AnnIndex` [:152](../../../src/handler/usearch/index.rs) | **안전.** 검색은 `Arc<VectorIndex>`를 들고 도므로([access/mod.rs:23](../../../src/handler/access/mod.rs)) 리더가 있는 동안 `drop`이 불릴 수 없다 |
| `unclaimed()` [:656](../../../src/handler/usearch/index.rs) | **안전.** 그래프가 받기를 거부했거나 `Unindexed`로 끝난 주소다([vector.rs:84](../../../src/handler/cmd/vector.rs)). `held`에 들어간 적이 없으니 어떤 검색도 그것을 내놓지 못한다 |
| `insert_published()` 오류 경로 [:597](../../../src/handler/usearch/index.rs) | **위반.** §3.2 |

### 3.2 고쳐야 하는 한 곳

```rust
let addr = match written {
    Ok(addr) => addr,
    Err(e) => {
        if let Some(old) = displaced {
            self.elements.release(&[old]);   // ← held에 아직 들어 있다
        }
        drop(held);
        drop(index);
```

`link()`가 실패한 경로다. `displaced`를 `held`에서 빼는 `give_up`은 성공 경로인
[:628](../../../src/handler/usearch/index.rs)에만 있으므로, 여기서 놓아주는 주소는 **아직
`held`에 있고 검색이 hit으로 내놓을 수 있다.** 게다가 `held` 쓰기 락과 그래프 읽기 락을 쥔
채로 `release`(= cache lock)를 부른다 — 트리거 설계 §6.2가 금지한 방향이다.

이 설계가 만드는 문제가 아니라 **지금도 있는 버그**다. epoch 방식에서도 `retire()`를
우회하므로 유예를 받지 않는다. 고침은 다른 둘과 같은 모양이다.

```rust
Err(e) => {
    let taken = displaced.is_some_and(|old| held.give_up(old, tombstone));
    drop(held);
    drop(index);
    let _ = self.drop_node(key);
    self.drop_displaced(displaced, taken);   // 큐로 간다
    return Err(PublishError::Store(e));
}
```

**이 수정이 선행되어야 한다.** 구현 계획의 첫 단계로 둔다.

## 4. 배리어

### 4.1 검색은 큐를 건드리지 않는다

배리어 하나를 **retire 사이에 들어온 검색 전부가 공유한다.** 검색은 열려 있는 배리어에
붙었다 떨어질 뿐이고, 그것이 큐에 들어가는 것은 retire가 봉인할 때뿐이다.

```
검색 R1 진입   열린 배리어에 +1                  큐: []
검색 R2 진입   열린 배리어에 +1                  큐: []
delete X      봉인(2) → 새 배리어를 연다          큐: [Barrier(2), Rel(X)]
검색 R3 진입   새 배리어에 +1                    큐: 그대로   ← R3은 Rel(X)를 막지 않는다
R1, R2 종료   봉인된 배리어가 2→0                sweeper 통과 → X release
```

봉인 시점에 붙은 검색이 **0이면 배리어를 아예 넣지 않는다.** 검색이 없는 동안의 delete는
큐에 `Release`만 남긴다.

### 4.2 검색은 자기가 붙은 배리어를 기억한다

봉인된 배리어는 큐로 가고 새 배리어가 열린다. 그래서 검색이 끝날 때 "지금 열린 배리어"에
`-1`을 하면 **엉뚱한 배리어**를 내리게 된다 — 봉인된 쪽은 영영 0이 안 되고 sweeper가 멈춘다.

합계로 갈음할 수도 없다. 검색은 끝나는 순서가 제각각이라, "끝난 검색 수"를 전역으로 세면
나중 배리어의 검색이 앞 배리어의 빚을 갚아버린다.

```
A, B가 배리어1에 붙음 → delete → 봉인(2), 큐로. 배리어2 개시
C가 배리어2에 붙음
B 끝(1), C 끝(2) → 합계 2 → 배리어1을 통과시킴 → Rel(X) 실행
                                              ← A는 아직 돈다. 터진다
```

그래서 검색은 **자기가 `+1`한 배리어가 어느 것인지**를 들고 다녀야 한다. 스코프 종료에 뭔가
실행되려면 가드 구조체가 어차피 필요하므로, 거기에 슬롯 번호를 넣는 것이 비용의 전부다.

```rust
struct Reading<'a> {
    owner: &'a Retirement,
    slot: usize,        // 내가 +1 한 슬롯. 스택에 있는 usize 하나
}
```

### 4.3 배리어는 고정 슬롯 배열에 둔다

`Arc<Barrier>`로 가리키면 코드는 짧아지지만, 공유 위치에서 `Arc`를 클론하려면 std에서는
락이 필요하다(`ArcSwap`이 없다). 검색마다 락을 잡게 되므로 쓰지 않는다. 배리어를 배열에
미리 깔고 **슬롯 번호로 가리킨다.** 봉인 경로가 cache lock을 쥔 unlink 콜백이라 할당이
없는 것도 이쪽이다.

```rust
struct Retirement {
    slots: [AtomicUsize; POOL],   // 배리어 하나 = 워드 하나
    open: AtomicUsize,            // 지금 열린 슬롯 번호 (0..POOL)
    queue: Mutex<Queue>,
    bell: Condvar,                // sweeper를 깨운다
    gate: Condvar,                // 대기 중인 검색을 푼다
}
```

### 4.4 봉인은 한 워드 안에서 끝낸다

배리어 한 칸에 **봉인 여부와 붙은 검색 수를 같이** 넣는다. 따로 두면 Dekker 패턴이 된다.

```
검색:  (1) count++            retire:  (1) sealed = true
       (2) sealed 읽기                 (2) count 읽기
```

`AcqRel`만으로는 **양쪽 다 상대를 못 보는** 실행이 허용된다 — 검색은 안 봉인됐다고 믿고
진행하는데 retire는 count 0을 보고 배리어를 안 넣는다. `SeqCst`를 써야 닫히고, 이것이 §1의
네 커밋과 같은 종류의 창이다.

한 워드에 넣으면 RMW **하나**가 둘을 동시에 보므로 창 자체가 없다.

```rust
const SEALED: usize = 1 << (usize::BITS - 1);
const COUNT:  usize = !SEALED;

// 검색 진입 — x86에서 lock xadd 하나. CAS 루프가 아니다
fn attach(cell: &AtomicUsize) -> bool {
    let prev = cell.fetch_add(1, Ordering::AcqRel);
    if prev & SEALED != 0 {
        cell.fetch_sub(1, Ordering::AcqRel);   // 봉인돼 있었다. 물러난다
        return false;
    }
    true
}

// retire 봉인 — 봉인과 동시에 그 순간의 카운트를 정확히 얻는다
fn seal(cell: &AtomicUsize) -> usize {
    cell.fetch_or(SEALED, Ordering::AcqRel) & COUNT
}
```

`fetch_add`의 반환값에 SEALED 비트가, `fetch_or`의 반환값에 그 순간의 카운트가 원자적으로
실려 온다. 한 워드에 전역 순서가 있으므로 **둘 중 하나는 반드시 상대를 본다.**

봉인된 슬롯에 붙으려다 물러난 검색은 `open`을 다시 읽는다. 물러나는 것은 `held`를 읽기
**전**이므로 그 검색은 아무 주소도 보지 못했고, 그래서 배리어가 필요 없다.

### 4.5 슬롯은 배리어가 큐에 들어갈 때만 넘어간다

슬롯은 유한하므로 재사용된다. **아직 큐에 있는 슬롯을 덮어쓰지 않는 것**이 유일한 규칙이고,
규칙을 지키는 방법은 하나다.

```
seal()이 0을 돌려주면  → 아무도 안 붙은 배리어다
                       → 큐에 넣지 않고, 슬롯을 0으로 되돌리고, open을 그대로 둔다
seal()이 0보다 크면    → 큐에 넣고, open을 다음 슬롯으로 넘긴다
```

이러면 **살아 있는 배리어 수 = 큐 안의 배리어 수**가 된다. 검색 없이 delete만 100만 번
들어와도 같은 슬롯을 봉인했다 되돌릴 뿐이라 `open`은 제자리다.

되돌리는 틈에 붙으려던 검색은 SEALED를 보고 물러났다가 같은 슬롯에 다시 붙는다. 그 검색은
`held.take()` **뒤에** 붙은 것이므로 그 주소를 못 보고, 배리어로 보호할 대상이 아니다.

**`POOL = CAP + 2`면 충분하다.** 큐가 `CAP`에 닿으면 게이트가 닫혀 새 검색이 안 붙으므로
(§6.2), 그때 붙어 있던 검색들이 마지막 배리어 하나를 만들고 끝이다. 그 뒤의 retire는
`seal()`이 0이라 배리어를 안 만든다. 큐 안의 배리어는 `CAP + 1`에서 멈춘다.

## 5. 큐와 sweeper

### 5.1 검색 진입과 종료

```rust
fn enter(&self) -> Reading<'_> {
    loop {
        let slot = self.open.load(Ordering::Acquire);
        if attach(&self.slots[slot]) {
            return Reading { owner: self, slot };
        }
        std::hint::spin_loop();      // 봉인 중이었다. open이 곧 넘어간다
    }
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        let prev = self.owner.slots[self.slot].fetch_sub(1, Ordering::AcqRel);
        if prev & COUNT == 1 && prev & SEALED != 0 {
            self.owner.ring_bell();  // 내가 마지막이고, 이 배리어는 큐에 있다
        }
    }
}
```

`Reading`은 `search()` **전체**를 덮는다. 위험 구간은 usearch 순회가 아니라 `resolve()`
안의 `elements.id_at(addr)`다 — 거기서 원시 포인터를 역참조한다
([index.rs:1103](../../../src/handler/usearch/index.rs)). 지금 `Searching` 가드가 덮는
범위를 그대로 물려받는다.

봉인되지 않은 배리어에서는 아무도 기다리지 않으므로 종을 울리지 않는다. 그래서 **평상시
검색 종료 비용은 원자 감산 하나**다.

### 5.2 retire

봉인과 push가 **한 덩어리**여야 한다. 나뉘면 retire 둘이 겹칠 때 순서가 뒤집힌다.

```
retire1: 봉인(b) → push [Barrier(b), Rel(X)]
retire2: 봉인(b1) → push [Barrier(b1), Rel(Y)]

섞여서 [Barrier(b1), Rel(Y), Barrier(b), Rel(X)] 가 되면
       b에 붙은 검색이 Y를 들고 있을 수 있는데 Rel(Y)를 안 막는다
```

retire는 원래 큐 뮤텍스를 잡으므로 그 안에서 다 한다. 검색은 이 뮤텍스를 잡지 않는다.

```rust
fn retire(&self, addrs: &[u64]) {
    if addrs.is_empty() { return; }
    let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);

    // open을 쓰는 것은 이 뮤텍스 아래뿐이다. 검색은 읽기만 한다
    let slot = self.open.load(Ordering::Acquire);
    if seal(&self.slots[slot]) > 0 {
        self.open.store((slot + 1) % POOL, Ordering::Release);
        q.push(Event::Barrier(slot));
    } else {
        self.slots[slot].store(0, Ordering::Release);   // §4.5
    }

    match q.events.back_mut() {
        // 꼬리가 Release면 칸을 늘리지 않고 주소만 붙인다
        Some(Event::Release(v)) if v.try_reserve(addrs.len()).is_ok() => {
            v.extend_from_slice(addrs)
        }
        _ => q.push_release(addrs),
    }

    q.arm_gate_if_full();
    drop(q);
    self.bell.notify_one();
}
```

### 5.3 sweeper

```rust
fn drain(&self) {
    loop {
        let batch = {
            let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
            match q.events.front() {
                None => { self.bell.wait(q); continue; }

                Some(Event::Barrier(slot)) => {
                    let s = *slot;
                    if self.slots[s].load(Ordering::Acquire) & COUNT > 0 {
                        self.bell.wait(q);                   // 안 풀렸다
                        continue;
                    }
                    self.slots[s].store(0, Ordering::Release);  // 재사용 준비
                    q.events.pop_front();
                    q.release_gate_if_room(&self.gate);
                    continue;
                }

                Some(Event::Release(_)) => {
                    let Some(Event::Release(v)) = q.events.pop_front() else { unreachable!() };
                    q.release_gate_if_room(&self.gate);
                    v                                        // 뮤텍스를 놓고 나서
                }
            }
        };
        self.elements.release(&batch);                       // 아무 락도 쥐지 않은 채
    }
}
```

`release`가 **아무 락도 쥐지 않은 채** 불리는 것이 구조에서 나온다 — 배치를 지역 `Vec`으로
꺼냈기 때문이다. `release` → `do_item_release`가 unlink 콜백을 재진입시켜도
([index.rs:840](../../../src/handler/usearch/index.rs)) 그 콜백은 큐 뮤텍스를 새로 잡고
자기 항목을 뒤에 붙일 뿐이다.

### 5.4 종은 뮤텍스를 거쳐 울린다

```rust
fn ring_bell(&self) {
    let _q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
    drop(_q);
    self.bell.notify_one();
}
```

sweeper가 "안 풀렸다"를 확인하고 `wait`에 들어가기 전에 종이 울리면 그 알림이 사라진다.
뮤텍스를 거치면 sweeper가 `wait`에 들어간 뒤에만 울린다.

이 경로는 **봉인된 배리어당 한 번**이다. 검색 경로의 비용이 아니다(§5.1).

## 6. 캡과 밸브

### 6.1 무엇이 큐를 키우나

sweeper가 **맨 앞 배리어에서 멈춘 동안에만** 쌓인다. 안 막혀 있으면 들어오는 대로 비운다.

막힌 동안 칸이 느는 조건은 하나다 — **retire 시점에 붙은 검색이 있을 때.** 검색이 없으면
배리어가 안 생기고 `Release`는 꼬리에 합쳐지므로(§5.2) 칸이 늘지 않는다. 그래서
`CAP = 1024`는 이런 뜻이 된다.

> 검색과 delete가 512번 번갈아 일어나는 동안 sweeper가 한 발도 못 뗐다.
> 즉 맨 앞 배리어의 검색 하나가 그동안 안 끝났다.

### 6.2 밸브는 검색 차단이다

```
검색 차단 → 진행 중인 검색이 끝남 → 배리어가 풀림 → sweeper 전진 → 큐가 빠짐
          → 그동안 오는 retire는 붙은 검색이 0이라 배리어를 안 만듦 → 칸이 더 안 늚
```

막힌 검색은 아무것도 붙잡고 있지 않으므로 데드락이 없다. 배리어에 붙기 **전에** 기다린다.

`CAP`은 **고수위 표시**다. retire는 unlink 콜백에서 오므로 cache lock을 쥐고 있어 기다릴 수
없고, 그래서 `CAP`에 닿아도 push는 그대로 진행되며 게이트만 닫힌다.

초과가 무한하지 않다는 것이 §4.5의 논거다. 게이트가 닫히면 새 검색이 안 붙으므로
**배리어는 최대 하나 더 생기고**, 그 뒤 retire의 `Release`는 전부 꼬리에 합쳐진다(§5.2).
즉 칸 수는 `CAP + 1`에서 멈춘다. `POOL = CAP + 2`가 그 상한을 덮는다.

큐 자체의 할당이 실패하면 그때는 기존 방식대로 로그를 남기고 그 배치를 포기한다(아이템이
프로세스 끝까지 묶인다).

검색 대기에는 타임아웃을 둔다. 만료되면 `SERVER_ERROR`로 답하고 데몬은 살아 있는다.

### 6.3 주소 수는 캡을 걸지 않는다

**엔진이 이미 속도를 제한하기 때문이다.**

`vdrop`은 `flush_prefix`를 쓴다. 데몬은 프리픽스의 `oldest_live`를 찍고 LRU의 **일부만**
즉시 unlink하며, 나머지는 나중에 접근될 때 만료된다
([kv.rs:261](../../../src/handler/arcus/engine/kv.rs)). 그 만료 회수도 연산당 상한이 걸려
있다 — `do_item_alloc`의 재활용 루프가 `tries`를 10 → 10+20 → 200 → 50으로 끊고
(arcus-memcached-EE `engines/default/item_base.c:807` 이하),
공간 부족 시의 `do_item_regain(current_ssl, ...)`도 `tries = current_ssl`로 끊는다
(같은 파일 `:721`).

그래서 unlink 콜백은 **한 번에 쏟아지지 않고 조금씩 온다.** 큐에 들어가는 주소는 retire당
한둘이고, 그 retire의 도착 속도 자체를 엔진이 묶는다. `vdrop` 직후에 그래프를 통째로
놓아주는 것은 큐가 아니라 `Drop for AnnIndex`이고, 그것은 §3.1대로 `Arc`가 보장한다.

주소가 한 번에 폭증할 수 있는 유일한 경로는 `clear_with()`인데(`take_all()` → `retire`),
**현재 호출자가 없다** — `begin_rebuild()`와 `drop_all()`이 부르지만
([index.rs:1017, 1022](../../../src/handler/usearch/index.rs)) 그 둘을 부르는 곳이 없다.
되살아나면 그때 그 경로만 배치로 나눠 넣으면 되고, 그것은 이 설계를 바꾸지 않는다.

한편 delete와 검색이 함께 계속 오는 경우는 retire마다 `[Barrier, Release]`가 붙으므로
**주소 수가 칸 수에 자연히 묶인다.** 두 경우 모두 칸 수 캡 하나로 덮인다.

## 7. 잠금 순서

트리거 설계 §6의 표에 한 줄도 더하지 않는다.

```
콜백      cache lock  →  그래프 락            (큐 뮤텍스. 그 안에서 다른 락을 안 잡는다)
검색      (원자 연산) →  그래프 락  →  (락 없이 포인터 읽기)
sweeper   (큐 뮤텍스를 놓고) cache lock
```

검색은 큐 뮤텍스를 **진입·종료 어느 쪽에서도 잡지 않는다** — 종을 울리는 경우만 예외이고
그것은 봉인된 배리어당 한 번이다(§5.4). sweeper가 `release`를 부를 때 아무 락도 없다(§5.3).
콜백이 잡는 큐 뮤텍스 안에는 다른 락이 없다.

## 8. `AnnIndex::epoch`는 건드리지 않는다

이름이 닮아 헷갈리기 쉬운데 **`AnnIndex::epoch`는 회수 기계가 아니다.** 올리는 곳이
[index.rs:1045](../../../src/handler/usearch/index.rs)의 `clear_with()` 하나뿐인 **인덱스
세대 카운터**다.

| 위치 | 묻는 것 |
|---|---|
| `resolve()`의 `epoch != entered` | 검색 도중 인덱스가 통째로 리셋됐나 |
| `insert_published()`의 `live` | stage와 publish 사이에 리셋이 있었나 |

배리어로 대체되지 않는다. `clear_with()`는 배리어를 기다리지 않으므로 검색 중에도 그래프를
비울 수 있다. 주소는 유효하게 남지만(release는 큐를 거치니까) 이전 세대의 답이다.
`held.contains()`로 갈음할 수도 없다 — 아이템 포인터는 엔진이 재사용하므로, 리셋 뒤 같은
주소에 다른 아이템이 올라오면 `contains()`는 통과하고 `id_at`이 **엉뚱한 id**를 돌려준다.
크래시가 아니라 조용한 오답이라 더 나쁘다.

`search()`의 `entered`, `resolve(&hits, entered)`의 인자, `Staged::at_epoch`가 전부 지금
모양 그대로 남는다.

## 9. 사라지는 코드

| 파일 | 대상 | 왜 |
|---|---|---|
| `index.rs` | `Searching`, `READER_SLOTS`, `readers[128]`, `unslotted`, `REGISTERING`, `NO_READER`, `HOME` thread_local | §4가 대체한다 |
| `index.rs` | `oldest_reader()`, `take_ready()`, `RECLAIM_BATCH`, `queued` | 유예 계산이 없어진다 |
| `index.rs` | `retired: Mutex<Vec<(u64, u64)>>` | `Retirement`의 큐가 대신한다 |
| `index.rs` | `pub fn reclaim()` | sweeper의 `drain()`으로 간다. 요청 스레드에서는 안 불린다 |
| `server.rs` | `COARSE`, `coarse_now()`, `tick()`, `advance_epoch()`, `advance()` | 남은 소비자가 없다 |
| `registry.rs` | `touch()`와 `last_access` | `coarse_now()`의 유일한 비-epoch 소비자였는데 **읽는 곳이 없다** |
| `sweep.rs` | 1초 틱의 `crate::server::tick()` | 시계가 없어진다 |
| arcus-memcached | `get_num_threads` server API | 슬롯 수를 맞추려던 목적이 사라진다 |

**남는 것 넷을 분명히 해둔다.** 이름이 겹쳐서 같이 지우기 쉽다.

| 남는 것 | 왜 |
|---|---|
| `AnnIndex::epoch` | 인덱스 세대 카운터 (§8) |
| `registry::CLOCK` | `remove_if_stale`의 stamp. 별개 카운터다 ([registry.rs:18](../../../src/handler/registry.rs)) |
| `in_flight` / `InFlight` | `k` 보정과 staging용. 배리어와 무관하다 |
| `stuck` / `retry_stuck()` | 그래프에서 안 빠진 노드의 재시도. sweeper의 유휴 경로에 얹는다 |

`sweep::retire(Option<Arc<VectorIndex>>)`(그래프 통째 폐기)와 `AnnIndex::retire(&[u64])`
(주소 폐기)는 이름이 같지만 다른 것이다. 둘 다 남는다.

## 10. 대가와 한계

1. **오래 도는 검색 하나가 그 뒤 전부를 막는다.** 맨 앞 배리어가 안 풀리면 그 뒤의 `Release`는
   전부 선다. 유예가 검색 단위가 아니라 큐 위치 단위라서 생기는 성질이고, §6의 밸브가 그
   상한을 준다.
2. **`CAP`이 새 튜닝 손잡이다.** epoch 방식에는 없던 것이고, 작게 잡으면 밸브가 자주 걸린다.
3. **밸브가 걸린 동안 검색이 선다.** 진행 중인 검색이 끝나는 시간이고, 타임아웃 뒤에는
   에러다. 지연이 아니라 실패로 드러나는 편이 낫다고 보고 고른 쪽이다.
4. **배리어 칸은 인덱스마다 `POOL`개 배열로 상주한다.** 1026 × 8바이트 ≈ 8KB. 인덱스가 아주
   많으면 합이 눈에 띌 수 있다.
5. **`unslotted`가 만들던 실패 모드는 사라진다.** 128 리더를 넘기면 회수가 아예 멈추던 것이
   없어진다.

## 11. 테스트

기존 테스트 중 `Searching::new`를 직접 쓰는 것들
([index.rs:1521, 1531](../../../src/handler/usearch/index.rs))이 `Reading`으로 바뀐다.

| 확인할 것 | 방법 |
|---|---|
| 앞선 검색이 살아 있으면 안 돌려준다 | `Reading`을 살려둔 채 retire + `drain()` 한 바퀴. `FAKE.id_at(addr)`이 살아 있는지 |
| 풀리면 돌려준다 | 가드를 놓고 한 바퀴. `id_at`이 `None`인지 |
| **나중에 시작한 검색은 안 막는다** | retire 후에 새 `Reading`을 열어둔 채 `drain()`. 그 `Release`가 통과하는지 (§2.1) |
| 붙은 검색이 0이면 배리어를 안 넣는다 | 검색 없이 retire 두 번. 큐가 `[Release]` 한 칸인지 (§4.1) |
| 꼬리 합치기 | 연속 retire가 칸을 안 늘리는지 |
| 검색이 자기 슬롯을 내린다 | 배리어가 봉인된 뒤 종료한 검색이 큐 안의 그 배리어를 0으로 만드는지 (§4.2) |
| 슬롯이 안 감긴다 | 검색 없이 retire를 `POOL`의 몇 배로 돌려도 `open`이 제자리인지 (§4.5) |
| 봉인 중 진입은 다음 슬롯으로 간다 | `seal()` 직후 `attach()`가 실패하고, `open`이 넘어갔거나 슬롯이 되돌려졌는지 (§4.4, §4.5) |
| retire 둘이 겹쳐도 순서가 선다 | 두 스레드가 동시에 retire. 큐가 `[B, R, B, R]` 꼴인지 (§5.2) |
| 종이 유실되지 않는다 | 마지막 검색의 퇴장과 sweeper의 `wait` 진입을 겹쳐 놓고 깨는지 (§5.4) |
| 게이트가 큐를 빼준다 | `CAP`을 작게 잡고 검색·delete를 섞어 돌려 큐가 줄어드는지 |
| §3.2가 큐를 거친다 | `link()` 실패 시 `displaced`가 즉시 release되지 않는지 |

`drain()`을 한 바퀴만 도는 형태로 나눠두면 위 대부분이 sweeper 스레드 없는 단일 스레드
테스트가 된다.

---

관련 문서: [2026-09-14-arcvector-item-trigger-design.md](2026-09-14-arcvector-item-trigger-design.md)
(§6 잠금 순서를 이 문서가 확장한다),
[2026-08-10-arcvector-usearch-design.md](2026-08-10-arcvector-usearch-design.md).
