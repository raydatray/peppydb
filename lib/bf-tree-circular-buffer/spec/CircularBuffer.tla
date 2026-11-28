--------------------------- MODULE CircularBuffer ---------------------------
\* Formal specification of bf-tree-circular-buffer v2 (plan 0038):
\*   - 8-byte intrusive block header in the slab (owner, span, state, flags)
\*   - O(1) doubly-linked per-class free lists stored in Freed payloads
\*   - Second-chance region (blocks freed/aged there become Tombstones)
\*   - Concurrent owner alloc/free and multi-evictor claim/retire/unclaim
\*   - Logical-address block identity (no sequence numbers)

EXTENDS Integers, FiniteSets

CONSTANTS
    HeaderBytes,    \* 8
    Capacity,       \* Power-of-two slab size, >= 2 * (MaxClass + HeaderBytes)
    Classes,        \* Set of payload sizes (multiples of 8, >= 16)
    SecondChance,   \* Distance threshold from tail; > Capacity disables region
    MaxTail         \* Finite bound on monotonic tail (covers multiple laps)

NONE == -1
DeadStates == {"Freed", "Tombstone", "Evicted", "Filler"}

VARIABLES
    \* Map from active logical block address to its in-slab header and
    \* (when state = "Freed") dead-payload links:
    \*   [span, state, class, freedWhileClaimed, prev, next]
    blocks,
    head,           \* Logical byte address of the oldest active block
    tail,           \* Logical byte address of the next bump allocation
    freeHead,       \* [Classes -> active logical address \cup {NONE}]
    held,           \* Ghost: logical addresses of live `Block` handles
    claimed         \* Ghost: map [addr |-> span] of active `Victim` claims

vars == <<blocks, head, tail, freeHead, held, claimed>>

\* --- Helpers ---

Offset(addr) == addr % Capacity
InRegion(addr, t) == (t - addr) >= SecondChance

FillerLen(t, span) ==
    LET off == Offset(t)
    IN IF off + span > Capacity THEN Capacity - off ELSE 0

Fits(span) ==
    (tail - head) + FillerLen(tail, span) + span <= Capacity

\* Remove key `k` from function `f`.
RemoveKey(f, k) == [x \in (DOMAIN f \ {k}) |-> f[x]]

\* Add or overwrite `k |-> v` in function `f`.
PutKey(f, k, v) == [x \in (DOMAIN f \cup {k}) |-> IF x = k THEN v ELSE f[x]]

\* Unlink a `Freed` block at logical address `a` from its class's doubly-linked list.
Unlink(bs, fh, a) ==
    LET m == bs[a]
        p == m.prev
        n == m.next
        b1 == [bs EXCEPT ![a].prev = NONE, ![a].next = NONE]
        b2 == IF p # NONE THEN [b1 EXCEPT ![p].next = n] ELSE b1
        b3 == IF n # NONE THEN [b2 EXCEPT ![n].prev = p] ELSE b2
        fh1 == IF p = NONE THEN [fh EXCEPT ![m.class] = n] ELSE fh
    IN [blocks |-> b3, freeHead |-> fh1]

\* Push block at `a` onto the head of its class's doubly-linked free list.
Link(bs, fh, a) ==
    LET c == bs[a].class
        old == fh[c]
        b1 == [bs EXCEPT ![a].state = "Freed", ![a].prev = NONE, ![a].next = old]
        b2 == IF old # NONE THEN [b1 EXCEPT ![old].prev = a] ELSE b1
        fh1 == [fh EXCEPT ![c] = a]
    IN [blocks |-> b2, freeHead |-> fh1]

\* Walk forward from `h` over dead blocks, unlinking `Freed` holes in O(1).
RECURSIVE Advance(_, _, _, _)
Advance(bs, fh, h, t) ==
    IF h = t THEN
        [blocks |-> bs, freeHead |-> fh, head |-> h]
    ELSE
        LET m == bs[h]
        IN IF m.state \notin DeadStates THEN
               [blocks |-> bs, freeHead |-> fh, head |-> h]
           ELSE
               LET u == IF m.state = "Freed" THEN Unlink(bs, fh, h)
                        ELSE [blocks |-> bs, freeHead |-> fh]
               IN Advance(RemoveKey(u.blocks, h), u.freeHead, h + m.span, t)

\* Pop from `freeHead[c]`, tombstoning holes that aged into the second-chance region.
RECURSIVE TakeHole(_, _, _, _)
TakeHole(bs, fh, c, t) ==
    LET a == fh[c]
    IN IF a = NONE THEN
           [found |-> FALSE, addr |-> NONE, blocks |-> bs, freeHead |-> fh]
       ELSE
           LET u == Unlink(bs, fh, a)
           IN IF InRegion(a, t) THEN
                  LET b1 == [u.blocks EXCEPT ![a].state = "Tombstone"]
                  IN TakeHole(b1, u.freeHead, c, t)
              ELSE
                  [found |-> TRUE, addr |-> a, blocks |-> u.blocks, freeHead |-> u.freeHead]

\* Walk forward from `a` to `t` to find the oldest `Live` block, or `NONE`.
RECURSIVE FindOldestLive(_, _, _)
FindOldestLive(bs, a, t) ==
    IF a = t THEN NONE
    ELSE IF bs[a].state = "Live" THEN a
    ELSE FindOldestLive(bs, a + bs[a].span, t)

\* --- Initial State ---

Init ==
    /\ blocks = [a \in {} |-> 0]
    /\ head = 0
    /\ tail = 0
    /\ freeHead = [c \in Classes |-> NONE]
    /\ held = {}
    /\ claimed = [a \in {} |-> 0]

\* --- Actions (each models one Mutex<State> critical section) ---

\* 1. Allocate a block of payload size `c`: reuse an eligible hole, else bump at tail.
Alloc(c) ==
    LET th == TakeHole(blocks, freeHead, c, tail)
    IN IF th.found THEN
           LET a == th.addr
           IN /\ blocks' = [th.blocks EXCEPT ![a].state = "Live",
                                             ![a].freedWhileClaimed = FALSE]
              /\ freeHead' = th.freeHead
              /\ held' = held \cup {a}
              /\ UNCHANGED <<head, tail, claimed>>
       ELSE
           LET span == HeaderBytes + c
               f == FillerLen(tail, span)
               addrB == tail + f
               newTail == addrB + span
           IN /\ Fits(span)
              /\ newTail <= MaxTail
              /\ LET b1 == IF f > 0 THEN
                               PutKey(th.blocks, tail,
                                   [span |-> f, state |-> "Filler", class |-> NONE,
                                    freedWhileClaimed |-> FALSE, prev |-> NONE, next |-> NONE])
                           ELSE th.blocks
                     b2 == PutKey(b1, addrB,
                               [span |-> span, state |-> "Live", class |-> c,
                                freedWhileClaimed |-> FALSE, prev |-> NONE, next |-> NONE])
                 IN /\ blocks' = b2
                    /\ freeHead' = th.freeHead
                    /\ tail' = newTail
                    /\ held' = held \cup {addrB}
                    /\ UNCHANGED <<head, claimed>>

\* 2. Owner returns a live `Block` handle at address `a`.
Free(a) ==
    /\ a \in held
    /\ LET m == blocks[a]
           step == IF m.state = "Evicting" THEN
                       [blocks |-> [blocks EXCEPT ![a].freedWhileClaimed = TRUE],
                        freeHead |-> freeHead]
                   ELSE IF a = head \/ InRegion(a, tail) THEN
                       [blocks |-> [blocks EXCEPT ![a].state = "Tombstone"],
                        freeHead |-> freeHead]
                   ELSE
                       Link(blocks, freeHead, a)
           adv == Advance(step.blocks, step.freeHead, head, tail)
       IN /\ blocks' = adv.blocks
          /\ freeHead' = adv.freeHead
          /\ head' = adv.head
          /\ held' = held \ {a}
          /\ UNCHANGED <<tail, claimed>>

\* 3. Evictor claims the oldest `Live` block (walking by span from head).
ClaimVictim ==
    LET adv == Advance(blocks, freeHead, head, tail)
        oldest == FindOldestLive(adv.blocks, adv.head, tail)
    IN /\ oldest # NONE
       /\ blocks' = [adv.blocks EXCEPT ![oldest].state = "Evicting"]
       /\ freeHead' = adv.freeHead
       /\ head' = adv.head
       /\ claimed' = PutKey(claimed, oldest, adv.blocks[oldest].span)
       /\ UNCHANGED <<tail, held>>

\* 4. Evictor retires a claimed block (valid only after the owner freed it).
Retire(a) ==
    /\ a \in DOMAIN claimed
    /\ blocks[a].freedWhileClaimed = TRUE
    /\ LET b1 == [blocks EXCEPT ![a].state = "Evicted"]
           adv == Advance(b1, freeHead, head, tail)
       IN /\ blocks' = adv.blocks
          /\ freeHead' = adv.freeHead
          /\ head' = adv.head
          /\ claimed' = RemoveKey(claimed, a)
          /\ UNCHANGED <<tail, held>>

\* 5. Evictor gives up a claim (retires if the owner freed it in the meantime).
Unclaim(a) ==
    /\ a \in DOMAIN claimed
    /\ IF blocks[a].freedWhileClaimed THEN
           Retire(a)
       ELSE
           /\ blocks' = [blocks EXCEPT ![a].state = "Live"]
           /\ claimed' = RemoveKey(claimed, a)
           /\ UNCHANGED <<freeHead, head, tail, held>>

Next ==
    \/ \E c \in Classes : Alloc(c)
    \/ \E a \in held : Free(a)
    \/ ClaimVictim
    \/ \E a \in DOMAIN claimed : Retire(a)
    \/ \E a \in DOMAIN claimed : Unclaim(a)

Spec == Init /\ [][Next]_vars

\* --- Safety Invariants ---

\* 1. Physical budget never exceeds Capacity.
BudgetBound ==
    /\ head <= tail
    /\ tail - head <= Capacity

\* 2. Head never passes a held Block handle or an active Victim claim;
\*    when nothing is held or claimed, the buffer is completely empty.
HeadSafety ==
    /\ \A a \in held :
           /\ a \in DOMAIN blocks
           /\ blocks[a].state \in {"Live", "Evicting"}
           /\ (blocks[a].state = "Evicting" => blocks[a].freedWhileClaimed = FALSE)
    /\ \A a \in DOMAIN claimed : a \in DOMAIN blocks
    /\ (held = {} /\ DOMAIN claimed = {}) => head = tail

\* 3. No active block wraps past Capacity, and no two active blocks share a
\*    physical slab byte (covers headers, live payloads, freed links, fillers).
NoPhysicalOverlap ==
    /\ \A a \in DOMAIN blocks :
           /\ blocks[a].span >= HeaderBytes
           /\ blocks[a].span % HeaderBytes = 0
           /\ Offset(a) + blocks[a].span <= Capacity
    /\ \A a1, a2 \in DOMAIN blocks :
           a1 # a2 =>
               LET o1 == Offset(a1)
                   o2 == Offset(a2)
               IN (o1 + blocks[a1].span <= o2) \/ (o2 + blocks[a2].span <= o1)

\* 4. Walking by span from `head` visits exactly DOMAIN blocks and lands on `tail`.
RECURSIVE Walk(_, _, _, _)
Walk(bs, a, t, visited) ==
    IF a = t THEN
        visited = DOMAIN bs
    ELSE IF a \notin DOMAIN bs \/ a \in visited \/ bs[a].span <= 0 \/ a + bs[a].span > t THEN
        FALSE
    ELSE
        Walk(bs, a + bs[a].span, t, visited \cup {a})

ContiguousWalk ==
    Walk(blocks, head, tail, {})

\* 5. Doubly-linked free lists have zero stale entries, zero leaks, symmetric links,
\*    and links appear ONLY inside `Freed` blocks (never touching a `held` payload).
FreeListIntegrity ==
    /\ \A c \in Classes :
           freeHead[c] # NONE =>
               /\ freeHead[c] \in DOMAIN blocks
               /\ blocks[freeHead[c]].state = "Freed"
               /\ blocks[freeHead[c]].class = c
               /\ blocks[freeHead[c]].prev = NONE
    /\ \A a \in DOMAIN blocks :
           LET m == blocks[a]
           IN IF m.state = "Freed" THEN
                  /\ a \notin held
                  /\ a \notin DOMAIN claimed
                  /\ (m.prev = NONE <=> freeHead[m.class] = a)
                  /\ (m.prev # NONE =>
                         /\ m.prev \in DOMAIN blocks
                         /\ blocks[m.prev].state = "Freed"
                         /\ blocks[m.prev].class = m.class
                         /\ blocks[m.prev].next = a)
                  /\ (m.next # NONE =>
                         /\ m.next \in DOMAIN blocks
                         /\ blocks[m.next].state = "Freed"
                         /\ blocks[m.next].class = m.class
                         /\ blocks[m.next].prev = a)
              ELSE
                  m.prev = NONE /\ m.next = NONE

\* 6. Address identity: every active claim names the exact same Evicting block
\*    (same span, never reused or freed into a hole) for the life of the claim.
ClaimIdentity ==
    \A a \in DOMAIN claimed :
        /\ a \in DOMAIN blocks
        /\ blocks[a].state = "Evicting"
        /\ blocks[a].span = claimed[a]
        /\ (blocks[a].freedWhileClaimed <=> a \notin held)
=============================================================================
