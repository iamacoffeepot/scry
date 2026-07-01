# scry — causal game analysis (design)

How scry turns one match's raw data into *understanding*: not a stats panel, but a
reconstruction of **what happened and why**. This is the design spec for the
analysis layer that feeds the `OVERVIEW` prompt (and, later, the poller).

Status: design. Grounded and validated against the archived match
`archive/NA1/5592214271` (Moon#132, Ezreal BOTTOM, a defeat, patch **16.13**).

---

## 1. Thesis

We analyze **relationships between events in time and space**, never standalone
magnitudes. "Vision score 14, 0.47/min" is dressing — nothing depends on it. "You
died in the enemy jungle at 24:01 with no trade, and Blue took Baron 40s later" is
understanding.

**The governing rule (applies to every fact we compute or surface):**

> A metric earns its place only if it *relates two events in time and space* — a
> death to a vision/position state, a pick to an objective, a lead to what it
> bought. A flat count, a per-minute rate, or a percentile-vs-population with no
> causal link is dressing. Cut it.

Corollaries:
- **No population benchmarks.** "Top-decile CS/min" is self-evident-or-noise (a
  Challenger's 3 wards/min tells a Gold player nothing actionable). The standard is
  *internal*: the opportunity this game presented, and whether the player took it.
  The only legitimate comparison is **opponent-relative** (you vs your role
  opponent at the 10/14/20-min frames) — that's causal, not population ranking.
- **Consequence-linked or cut.** A stat appears only if we can point to the moment
  in *this* game where it cost or won something.
- **Loaded terms require proof.** first blood / ace / Soul / steal only when the
  data explicitly confirms it (already enforced in `prompts/OVERVIEW.md`).

---

## 2. Data tiers

Two sources, very different cost and fidelity. Design everything so the analysis is
**source-agnostic**: the same moment catalog runs on either tier; richer inputs
just upgrade a join's confidence.

### Tier 1 — match-v5 API (what scry uses today)

`match.json` (full detail + `challenges`) + `timeline` (per-minute frames + exact-ms
events). This is what `--dump` archives. Everything in §4–§6 is computable from
Tier 1 unless marked otherwise.

**Timeline shape:** frames at a **60 000 ms** interval. Each frame has
`participantFrames` (`totalGold`, `currentGold`, `xp`, `level`, `minionsKilled`,
`jungleMinionsKilled`, `position{x,y}`, `championStats`, `damageStats`) and an
exact-ms `events[]` list.

**Hard limits of Tier 1** (validated against the archive):
| Limit | Consequence |
|---|---|
| **Ward events carry no position** (`WARD_PLACED`/`WARD_KILL`: only `wardType` + creator/killer id; no `wardId`) | Vision is a *temporal* join only. "Was a ward active when you died" is approximable; "did it cover where you died" is **out of reach in Tier 1**. |
| **60s frame granularity** | All position-derived signals (isolation, roam, wave position) are ≤59s stale between frames. Exception: events that carry their own `position` are frame-exact. |
| **No ability casts, no summoner-spell usage** | Can't detect a burned Flash/TP as trade value (except a TP shows as an impossible position jump between frames). |
| **Region/lane classification is DIY** | Only `BUILDING_KILL`/`TURRET_PLATE_DESTROYED` carry `laneType`. Kills/monsters give raw `{x,y}`; zone polygons + pit coords must be hand-authored and **validated against our own archive** (community-derived coords are not official). |

**Positioned event types** (frame-exact `{x,y}`), the backbone of every spatial
join: `CHAMPION_KILL`, `CHAMPION_SPECIAL_KILL`, `ELITE_MONSTER_KILL`,
`BUILDING_KILL`, `TURRET_PLATE_DESTROYED`. Everything else (wards, items, levels,
skills) is positionless.

### Tier 2 — `.rofl` replay files (future, heavier)

The encrypted replay file contains the **full high-frequency positional stream,
ward positions included** — exactly the data Tier 1 omits. It lifts the ward-position
wall and the 60s-staleness wall. Cost: fetch the patch-matched `.rofl` (client /
spectator endpoint), decrypt it, and parse the reverse-engineered keyframe/chunk
binary — a separate, client-dependent pipeline.

**We do not build Tier 2 now.** We design the contract so that when replay-derived
positions are available, the spatial vision joins (ward-covers-death, precise
isolation, exact wave position) upgrade **Proxy → Solid** with no change to the
moment catalog — only the confidence and the inputs change.

### Cheap Tier-1 wins to bank first
- **Stop slimming `currentGold`.** `archive/mod`'s `slim_frame` drops it; the raw
  timeline has it. Restoring it makes **unspent-gold-at-death** and shutdown-value
  weighting computable (currently impossible in our archive only because we threw
  the field away). One-line fix in `crates/scry/src/archive.rs`.
- **Validate pit / lane-zone coordinates** against our own archived `position`
  values before hardcoding any (dragon pit ≈ (9863, 4416) and bot-tower anchors are
  already confirmed present in this match).

---

## 3. Architecture: the derived-fact contract

Today the prompt is handed raw JSONL and asked to eyeball the timeline. Replace that
with a Rust **analysis pass** that computes grounded facts, so the model reasons over
*conclusions*, not raw events. Two products:

1. **State functions** — time-indexed reconstructions of game state:
   `power_delta(p, opp, t)`, `objective_live(type, t)`, `camps_up(side, t)`,
   `push_state(lane, t)`, `in_base(p, t)`, `vision_uptime(team, t)`. (§4)
2. **Moments** — classified event-joins, each already labelled and cited:
   `{ kind, t, position?, actors, evidence[], severity }`. (§5)

The prompt then receives a compact, pre-grounded brief (the ranked moments + the
state snapshots at each) instead of 1135 raw events. This is more grounded (the
model can't miscount), cheaper (less context), and testable (the joins are code with
fixtures). The `OVERVIEW` prompt's job narrows from "find the story" to "narrate the
already-found story in League-literate prose."

Everything below is **patch-keyed**: every timer/threshold reads from a table indexed
on `gameVersion` (this match is 16.13). A hardcoded constant silently rots when Riot
reships Baron/Herald/cannon timing — and the match's own kill timestamps *cannot*
back out the timers, so the table must be authoritative, not inferred.

---

## 4. Derived game-state layer

Time-indexed state you reconstruct from timers + events. Class:
**DETERMINISTIC** (exact from timers/events) / **ESTIMABLE** (modeled, bounded error)
/ **GAP** (needs Tier 2 or another source).

| State | Class | Method | Caveat |
|---|---|---|---|
| **Power spikes** (lvl 6/11/16, item completions) | DETERMINISTIC | `LEVEL_UP` where level∈{6,11,16}; `ITEM_PURCHASED` of a *terminal* item (patch Data Dragon: empty `into`/`depth≥3`) | which spikes *matter* needs champ knowledge. Real: Lux out-levels Ezreal at every breakpoint (L11 by 2+ min) — defensible "she had tempo." |
| **Objective spawn windows** | DETERMINISTIC *given patch table* | `next = last_kill.t + respawn`; first spawn fixed | can't infer timers from kills; exclude `killerTeamId∈{0,300}` sentinels (neutral despawn — the 19:45 Herald was unclaimed) |
| **Recall / in-base** | ESTIMABLE-strong | `ITEM_PURCHASED` ⇒ at fountain; corroborate with `position`≈fountain + flat gold slope | a no-buy back between two frames is invisible |
| **Unspent gold at T** | DETERMINISTIC* | `currentGold` (once un-slimmed) or `totalGold − Σpurchases + Σsells` | *needs the currentGold fix (§2); handle `ITEM_UNDO` |
| **Camp availability** | ESTIMABLE | per-camp state machine: `down` at the frame a jungler's `jungleMinionsKilled` steps up, `up` again after respawn timer; attribute camp by jungler `position` | ±45s (60s frames); small-camp attribution is a guess; enemy invades credit *their* jungler's tally |
| **Minion wave state** | DETERMINISTIC (baseline) + ESTIMABLE-strong (push) | see below | sub-60s position between corrections is interpolated — the *only* genuine residual |

### Wave state — the model that matters

Not a noisy CS heuristic. The wave is a **deterministic conveyor corrected by
positioned ground-truth events:**
- **Spawn + movement is exact.** Wave N leaves each nexus at `1:05 + (N−1)·30s`
  (patch-keyed cannon cadence), fixed path/speed ⇒ two equal waves meet at the lane
  midpoint. "What wave is out and where it meets by default" is *computed, not
  estimated*.
- **Perturbations are observable, not inferred.** A wave's state changes only when
  champs/towers interfere — and the decisive interference is positioned + exact:
  `TURRET_PLATE_DESTROYED` (66 in this game) and `BUILDING_KILL` fire **only when a
  wave was shoved onto that tower**. Each is hard proof "the wave was crashed at
  tower (x,y) at time T." Run the sim at equilibrium and snap it to the tower each
  event names, refined by forward champ `position` + CS slope.
- **Semantic that's easy to get wrong:** in `BUILDING_KILL`/`TURRET_PLATE_DESTROYED`,
  `teamId` is the tower's **owner/victim**, not the shover. A plate on a Red tower ⇒
  **Blue was shoving.**

Worked (bot lane, this game): opens at equilibrium (min 2–4), swings to a Blue crash
on Moon's tower at ~5:00 (plate at 5:35 confirms the champ-position read), oscillates
as both ADCs trade backs, Moon's side lands counter-crashes (plates 9:21, 13:40),
Blue wins the exchange race and takes the bot outer at 19:00. Every transition
anchored to a timestamped, coordinate-bearing event.

State functions compose into a "why": sample every function at a moment's `(t,x,y)` —
was the lane pushed? enemy camps up so their jungler was free? an objective live
nearby? power behind? — and the conjunction *is* the explanation.

---

## 5. Moment catalog

Event-joins, ranked by coaching signal. Each: rule → thresholds → what fired in the
validation game. Emit as `Moment` records with evidence and citations.

### 1. Fight → objective conversion — HIGHEST signal
The spine of "what decided the game."
- **Rule:** cluster `CHAMPION_KILL`s (new cluster when gap > 20s). A cluster is a won
  fight for team T if T has ≥2 kills and enemy ≤1. **Converted** if T takes an
  `ELITE_MONSTER_KILL` or enemy `BUILDING_KILL` within 90s; else **squandered**.
- **Thresholds:** 20s cluster gap, 90s conversion window. Attribute each objective to
  the *nearest single* preceding fight (avoid double-credit). Gate out post-decision
  garbage-time clusters.
- **Fired:** Red (Moon's team) **won 4 of its first 5 skirmishes and converted only
  one**; Blue converted *every* mid/late fight into towers/dragons. **That
  conversion gap — not laning — is why Red lost.** This is the headline.

### 2. Objective control / monopoly — HIGH
- **Rule:** per `ELITE_MONSTER_KILL`, side = `killerTeamId`; player participated if
  `killerId==p || p∈assistingParticipantIds` (distance ~1300u fallback). Flag
  monopoly when one team takes 100% of a type. Exclude `killerTeamId∈{0,300}`.
- **Fired:** all **4 dragons by one Graves** → Soul + monopoly; Baron 28:40 Blue.
  Moon was **never within participation of a single dragon or Baron** — for a bot
  carry, total absence from the objective dance is a clean, citable pattern.

### 3. Death quality — HIGH, actionable
- **Rule:** per death, **free** if no allied kill within ±10s and ≤2000u (else
  **traded**); **isolation** = distance to nearest living ally (interpolated frame);
  weight losses by `bounty`+`shutdownBounty`. Prefer an asymmetric window (−3s/+10s)
  so a kill landing just before your death doesn't false-count as a trade.
- **Fired:** **6 of 10 deaths were free**; two early ones isolated deep on the enemy
  side (throwaways before any fight). `shutdownBounty>0` = you funded their comeback.

### 4. Nemesis / repeat-killer — cheap, high-signal
- **Rule:** group a player's deaths by `killerId`; flag when one enemy ≥50%.
- **Fired:** **Akshan killed Moon in 6 of 10 deaths**, 4 of them free. Composes with
  death-quality into one coaching thread.

### 5. Lead → translation — often a *negative* finding (that's the point)
- **Rule:** team + lane gold/xp deltas at the 10/14/20-min frames + `challenges
  .laningPhaseGoldExpAdvantage`. Only claim a "lead" at ≥~1500 team gold or ≥1 level
  on the opponent; below that, emit "even game" — never a false "threw a lead."
- **Fired:** dead-even through 14:00 (`laningPhaseGoldExpAdvantage = 0`); the rule
  correctly *declines* to invent a snowball. The restraint is a feature.

### 6. Vision ↔ death — WEAK in Tier 1, temporal only
- **Rule (Tier 1):** own ward "active" if a `WARD_PLACED` by the player is within its
  assumed type-duration window before the death; team vision = wards placed in the
  prior 120s. **No positions**, so "a ward across the map counts the same as one on
  the death spot."
- **Verdict:** **cut per-death vision correlation as coaching text in Tier 1** — it
  fires on noise. Surface only the bare behavioral fact ("no personal ward for 10+
  min mid-game"). **This whole family upgrades to Solid in Tier 2** (real ward
  positions → true "were you warded when you died").
- Higher-value vision signal that *is* Tier-1 Solid: **objective vision setup** —
  friendly wards placed + enemy wards cleared in the 60–90s *before* an
  `ELITE_MONSTER_KILL`, then whether the team converged and took it.

### Turning points — the win-probability backbone
- Build a per-frame **win% series** from a feature vector that is entirely
  Tier-1-computable (Riot/AWS's published model: game time, gold %, team XP, players
  alive, tower/dragon counts, soul, Herald, inhibitor/Baron/Elder timers). The
  **largest swing that sticks = the throw**, attributed to the co-located
  kill/objective cluster. Strictly better than a gold graph — a Baron-while-behind
  moves win% more than it moves gold. This drives the `Turning Points` section.

---

## 6. What stays out of reach (Tier 1)

State these honestly rather than fake them:
- **Ward coverage of a location** — no ward positions (Tier 2 lifts this).
- **Sub-60s position** between frames/events — interpolated only.
- **Summoner-spell usage** (Flash/TP burned as trade value) — no cast events.
- **Intent.** Never label a player *inting/griefing* (intent is undetectable). Use
  "free death," "avoidable death," "caught out." *Feeding* (unintentional) is fine.
- **`challenges` fields** are useful *corroboration* (`soloKills`,
  `visionScoreAdvantageLaneOpponent`, `laningPhaseGoldExpAdvantage`) but have opaque,
  patch-variable definitions — never ground truth.

---

## 7. Build order

1. **Bank the cheap wins:** un-slim `currentGold`; validate pit/zone coords against
   the archive. (§2)
2. **Analysis pass skeleton:** the `Moment` type + the fight→objective, death-quality,
   nemesis, and objective-monopoly joins (all Tier-1 Solid, all validated above).
   Feed the ranked moments into `OVERVIEW` as pre-grounded facts.
3. **State functions:** power spikes, objective windows, wave state, camp
   availability — patch-keyed table first.
4. **Win-probability series** → turning points.
5. **(Later) Tier 2 replay pipeline** — unlocks the spatial vision family.

Validation harness: `archive/NA1/5592214271` is the golden fixture — every join's
expected output above is the assertion set.
