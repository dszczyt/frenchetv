# Channel guide: row-per-channel EPG with live previews

**Date:** 2026-09-19
**Status:** Design approved, not yet implemented

## Problem

The channel list is a 4-column grid of logos. It shows what channels exist and
nothing about what is on them. There is no way to see the schedule, and no way
to see what a channel is currently showing without tuning to it.

The goal is a guide: one row per channel, the channel logo and a small live
view of that channel on the left of the row, and the schedule laid out along a
time axis to the right.

## Starting state

Three facts about the repository shaped this design.

**There is no EPG data.** `Operator::fetch_epg()` returns `Ok(None)` for both
Orange (`orange.rs:1047`) and Bouygues (`bouygues.rs:749`).
`epg::xmltv::parse_xmltv()` is a six-line stub that returns an empty
`EpgData`. The types exist — `EpgProgram` with start/stop, `EpgData`,
`EpgData::current_program` — and nothing fills them. `quick-xml` is already a
dependency of `frenchetv-core`, added for a parser that was never written.
CLAUDE.md's screen flow lists an "EPG Grid" screen; it does not exist in either
UI.

**DRM is per-stream.** `StreamUrl::protection` is an `Option<ProtectionData>`,
so some channels are Widevine-protected and some are clear. The per-channel
cost of showing live video is one DRM proxy instance (`proxy::start` binds a
fresh port per call), one Widevine license exchange, and one decoder session.
That cost, not pixel throughput, is what does not scale.

**The two UI crates mirror each other by copy, and drift.**
`ui-desktop/src/widgets.rs` has no Android counterpart.
`ChannelListAction::SelectChannel` boxes its `Channel` on desktop and did not
on Android until 2026-09-19, because host-target clippy never linted the
`cfg(target_os = "android")`-gated crate. Both crates are on identical
egui 0.30 / egui_extras 0.30.

## Decisions

| Question | Decision |
|---|---|
| Preview scope | A live view in **every visible row**, not just the focused one |
| Preview fidelity | **Hybrid** — focused row plays real video; other visible rows show their most recent captured frame |
| Schedule source | **Public XMLTV feed**, behind an operator-first provider chain |
| Platforms | **Both** desktop and Android, together |
| Screen role | **Replaces** the 4-column grid |
| Capture strategy | **Neighbour prefetch + round-robin** from the start |

### Why hybrid rather than N live decodes

Every visible row showing genuinely moving video requires N concurrent decoders
and N Widevine sessions. On the Fire TV Stick — armv7, ~1GB RAM, and a secure
MediaCodec path that generally permits a single concurrent Widevine session —
that is not achievable. The hybrid still satisfies the requirement that every
visible row shows live television: the focused row moves, and the rest show
recent frames of their actual live output, refreshed on rotation.

### Why XMLTV rather than operator EPG APIs

One parser serves every operator, including channels that came from the M3U
fallback path. It matches the existing `xmltv.rs` stub and the `quick-xml`
dependency already present. Operator-native EPG remains preferred if anyone
reverse-engineers it — see the provider chain below.

## Architecture

```
crates/core/                       no UI, wiremock-testable
  epg/
    xmltv.rs      real streaming parser (quick-xml)
    source.rs     fetch + TTL disk cache (mirrors logo_cache.rs)
    index.rs      per-channel interval index
    matcher.rs    XMLTV id <-> operator channel matching
    provider.rs   operator-first, XMLTV-fallback chain
  preview/
    scheduler.rs  pure capture-priority policy, no I/O

crates/ui-shared/                  NEW crate, cfg-free pure egui
  guide/          rows, D-pad navigation, timeline layout
  widgets.rs      focus_ring and friends, moved from ui-desktop
  preview.rs      PreviewCache: channel_id -> TextureHandle + captured_at

crates/ui-desktop/   impl PreviewCapture via mpv
crates/ui-android/   impl PreviewCapture via ExoPlayer/JNI
```

Two seams carry the design:

```rust
/// The only platform-specific part of previews. Non-blocking.
trait PreviewCapture {
    fn request(&mut self, channel: &Channel);
    fn poll(&mut self) -> Vec<(ChannelId, egui::ColorImage)>;
    fn cancel(&mut self);
}

/// core::preview — a pure function of state.
fn next_target(view: &ViewState, cache: &dyn CacheView, now: Instant)
    -> Option<ChannelId>;
```

Both renderers differ (wgpu on desktop, glow on Android) but
`egui::Context::load_texture` is renderer-agnostic. If capture backends emit
raw RGBA as `ColorImage`, frame delivery is identical on both platforms. That
is what makes building for both at once affordable rather than double work.

`ui-shared` is deliberately narrow: the guide screen and `widgets.rs` only.
The other screens stay duplicated. Consolidating them is a separate refactor
with its own risk.

Because `ui-shared` is cfg-free, the existing host-side
`cargo clippy --workspace` and `cargo test --workspace` cover it. The most
complex screen in the app lands inside the CI gate rather than in the Android
blind spot.

## EPG pipeline

### Provider chain

```
EpgProvider::fetch(operator, channels, window)
    -> try operator.fetch_epg(hours)   // returns None today, both operators
    -> fall back to the XMLTV feed
```

This costs nothing now and mirrors the mandatory-M3U-fallback rule that already
governs channel lists, rather than introducing a second unrelated pattern. If
an operator's native EPG is reverse-engineered later it becomes the preferred
source with XMLTV as the fallback, with no structural change.

### Parser (`xmltv.rs`)

Streaming `quick-xml` reader, not DOM. A French feed covering ~200 channels
over 7 days is tens of megabytes and must not be held as a tree on a 32-bit
device with ~1GB of RAM. Programmes are materialized only for channels that
matched and that fall inside the requested window; everything else is skipped
without allocating.

XMLTV timestamps are of the form `20260919203000 +0200` and parse with chrono
as `%Y%m%d%H%M%S %z`.

A malformed feed yields whatever parsed successfully plus a warning. Partial
EPG beats none.

### Source and cache (`source.rs`)

Mirrors `logo_cache.rs` — TTL disk cache under the OS cache directory, sha1
keyed by feed URL — with two deviations:

- Cache the **parsed, filtered** result, not the raw XML, so cold start does
  not re-parse tens of megabytes.
- **Stale-while-revalidate**: serve the stale cache immediately and refresh in
  the background. The guide must never block on the network, and on the Fire TV
  the feed fetch is the slowest operation in the feature.

The feed URL lives in `config.toml` with a default. Feeds move and die; it is
not hardcoded. The chosen feed is documented in `docs/operators.md`.

### Matcher (`matcher.rs`)

XMLTV channel IDs do not match operator channel IDs. Both sides are normalized
— lowercased, accents stripped, `HD` / `UHD` / `4K` / `+1` suffixes removed,
whitespace collapsed — and operator `Channel.name` is matched against XMLTV
`<display-name>`, with a manual override table for names that do not normalize
cleanly.

The matcher **returns a report of unmatched channels**. A silent mismatch is
indistinguishable from "no schedule available" and is otherwise very hard to
diagnose.

### Index (`index.rs`)

`EpgData` is currently a flat `Vec<EpgProgram>` with a linear scan in
`current_program`. A guide drawing 5 rows over a 2-hour window re-scans it
every frame; at ~100k programmes that is unusable.

Replace the backing store with `HashMap<ChannelId, Vec<EpgProgram>>`, each
`Vec` sorted by start time. A row lookup is a hash plus a binary search to the
window start, then a forward walk while `start < window_end`.
`EpgData::current_program` keeps its existing signature.

## Preview engine

### Scheduler (`core::preview::scheduler`)

Priority order:

1. The focused row, if its frame is missing or stale
2. Neighbours within `prefetch_radius` of focus, nearest first
3. Other visible rows
4. Otherwise `None` — idle rather than burn the decoder

Missing beats stale at every tier. Ties break oldest-first. A channel already
in flight is never returned, nor is one fresher than `fresh_ttl`.

**Focus-settle debounce is a requirement, not polish.** Holding the D-pad moves
focus roughly ten rows per second while a single capture costs seconds. Without
debounce, capture is cancelled and restarted continuously and *nothing ever
completes* during exactly the interaction the feature exists for. The scheduler
targets the focused row only once focus has been stable for 300-500ms, serving
cached frames in the meantime.

`next_target` performs no I/O, owns no clock, and touches no decoder. The
riskiest reasoning in the feature is therefore a table test that runs anywhere.

### Cache (`ui-shared::preview`)

LRU bounded by entry count (~40), each entry a `TextureHandle` plus
`captured_at`. Frames are captured and stored at **256x144**, downscaled at
capture time rather than render time: ~147KB per entry, ~6MB worst case.
Implements `CacheView` for the scheduler.

### Backends

**Desktop (mpv).** A second, headless mpv instance at the lowest DASH
representation renders to an offscreen FBO; read pixels once, tear down.
`proxy::start` already binds a fresh port per call, so per-capture DRM proxies
work with no change.

**Android (ExoPlayer).** A second ExoPlayer instance writes to an
`ImageReader`; grab one frame, release. See the risk below.

### Budget and lifecycle

- `max_in_flight = 1` on both platforms initially
- Capture timeout ~8s, then mark failed and back off that channel
  exponentially, so a permanently broken channel cannot monopolize rotation
- Leaving the guide cancels all captures and drops proxies
- The focused row's real player is separate from the capture pipeline and
  always wins the decoder budget
- The focused row is already decoding; sample its frame into the cache every
  ~5s at no additional cost

**Invariant: capture yields to playback.** If capture ever starves the focused
player, the user sees stutter in the stream they are actually watching. Any
playback stall signal cancels in-flight capture and backs off. No preview is
worth a dropped frame in live playback.

### Known optimization, not built up front

Each capture currently repeats a full Widevine license exchange, which dominates
capture cost. Caching keys per channel across captures would speed rotation
substantially; the CDM already exists in `ui-desktop`. Reach for this only if
measured rotation cadence proves too slow.

## Guide screen

### Layout

The Fire TV Stick reports 1920x1080 at density 2.0, giving egui 960x540
**logical** points:

```
header: logo + clock + category tabs    ~70pt
time ruler                              ~28pt
rows viewport                          ~410pt
hint line                               ~32pt
                                        540pt
```

A 256x144 preview is 128x72 logical, so row height is ~80pt and **5 rows are
visible on the Fire TV** (~12 on a 1080p desktop window). That bounds the
scheduler's working set to roughly 9 channels — 5 visible plus a prefetch
radius of 2.

```
┌─────────────────────────────────────────────────────────────┐
│ [logo]  Tout  Généraliste  Info  Sport  …          21:58    │
├──────────────┬──────────────────────────────────────────────┤
│              │ 21:30      22:00      22:30      23:00       │
├──────────────┼──────────────────────────────────────────────┤
│ [TF1][still] │ │Journal │ Film: Les Visiteurs        │      │
│ [F2 ][ live] │ Envoyé spécial      │ Complément d'enq │     │  <- focused
│ [M6 ][still] │ │Capital │ Série            │ Météo    │     │
│ [ART][still] │ Documentaire   │ Cinéma de minuit      │     │
│ [C+ ][still] │ │Sport   │ Match: PSG-OM               │     │
└──────────────┴──────────────────────────────────────────────┘
   190pt fixed        770pt, ~2h at ~6.4pt/min        ^ now-line
```

The left block is fixed width (~190pt: logo plus preview); the timeline takes
the remainder. Programme blocks are positioned by start/stop and clipped to the
window; a programme that began before the window start is clipped with its
title still visible.

### Navigation

Extends the existing `FocusLayer` idiom from `channel_list.rs`:

- **Up/Down** — change row. Up from the top row moves to the category tabs.
- **Left/Right** — move programme-by-programme within the row; the time window
  scrolls on reaching its edge.
- **Enter** — tunes to the focused **row's** channel, regardless of which
  programme column is highlighted.

Enter always tuning is deliberate. Giving Enter a different meaning on a future
programme implies reminders, which is a subsystem this feature does not need.
The programme cursor exists to read the schedule; the detail panel shows the
highlighted programme's description.

### Virtualization

`ScrollArea::show_rows` renders only the visible range, and the range it
computes **is** the `ViewState.visible` the scheduler consumes — one source of
truth rather than two drifting notions of what is on screen. The focused row
calls `scroll_to_me`.

### Never-blank rule

- No captured frame yet: channel logo centred on dark fill, never an empty hole
- No EPG match: channel name plus "Programme non disponible"
- `locked` channels keep their existing hidden-by-default behaviour

### What it replaces

`channel_list.rs` is deleted from both crates and its category-filter logic
moves into the shared guide. The `ChannelListAction::SelectChannel` contract
into `app.rs` is unchanged on both platforms, so stream resolution, the player,
and the whole tune-in path are untouched. The blast radius stays in the screen
layer.

## Error handling

Follows CLAUDE.md's existing rules. EPG failures degrade silently: feed
unreachable falls back to stale cache, and with no cache the guide still
renders rows, logos, and previews with empty schedule columns. Stream errors
keep the existing retry overlay; auth errors keep the existing Setup redirect.

Previews are decoration. A capture failure means logo plus exponential backoff
and is never surfaced as an error.

## Testing

Offline, in CI:

| Unit | Coverage |
|---|---|
| `xmltv.rs` | fixture parse, malformed input, timestamp and timezone edges |
| `matcher.rs` | table tests over awkward real French channel names |
| `index.rs` | window boundaries, programme spanning window start, unordered input |
| `scheduler.rs` | priority order, debounce, in-flight exclusion, backoff |
| `source.rs` | wiremock: fresh, stale, 404, timeout, malformed |

`scheduler.rs` carries the whole risk surface of the prefetch strategy and
needs no device, no Widevine, and no decoder to test.

On-device, not automatable:

- Android `ImageReader` readback from a Widevine stream (see risk below)
- Rotation staleness with 5 visible rows
- Playback stutter while capture rotates
- The 540-point layout on the real panel, verified by screencap

## Principal risk: Android secure-surface readback

For Widevine L1 content the decoded frame lives on a **protected surface**,
which generally cannot be read back — `ImageReader` returns black or throws.
If that holds on this device, the Android preview path as designed does not
work.

This is an empirical question, not a design question. It is settled by one
`ImageReader` readback attempt against a Widevine channel over adb, which takes
minutes. **Run it during phase 1**, not phase 5: it determines what phase 5 is,
and finding out four phases deep is the expensive version.

Fallbacks, in order of preference:

1. Request **Widevine L3** for previews (software decode, readable) while the
   focused player keeps L1. Quality is irrelevant at 256x144.
2. Previews only for channels whose `StreamUrl::protection` is `None`; logo for
   the rest.
3. Android degrades to focused-row-only preview.

## Phasing

Each phase ends somewhere shippable.

1. **EPG pipeline in core.** No UI. Ends with real `EpgData` for real channels,
   proven by offline tests. Run the `ImageReader` measurement during this phase.
2. **`ui-shared` plus the guide screen, logos only, no previews.** Replaces the
   grid on both platforms. This alone is a better application than today: rows,
   schedule, navigation, tuning.
3. **Scheduler and `PreviewCache`.**
4. **Desktop capture backend.** mpv already renders to an egui texture, so this
   validates the preview design where iteration is fastest.
5. **Android capture backend**, gated on the measurement from phase 1.

The riskiest unknown is last and isolated. If Android readback is blocked,
phases 1-4 have still delivered a complete guide with schedule and desktop
previews, and phase 5 degrades to one of the documented fallbacks without
disturbing anything built before it.

## Out of scope

- Reminders, recording, or any action on a future programme
- Reverse-engineering operator-native EPG APIs (the provider chain leaves room
  for it)
- Moving the remaining screens into `ui-shared`
- Moving the DRM stack out of `ui-desktop` into `core`, despite CLAUDE.md's
  stated architecture — Android consumes none of it via ExoPlayer, and that
  code is the most delicate in the repository
