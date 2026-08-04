# DASH live-refresh demuxer bug — repro harness

`mock_live_dash.py` is a self-contained DASH server (no CDN, no `ui-desktop`
proxy, no decrypt path) that reproduces the "audio loops, video ok" bug
reported against Orange DASH-CE streams. It proves the bug is in ffmpeg's
dash demuxer's handling of periodic manifest refresh on a live (`type="dynamic"`)
MPD — not in this app's proxy, CENC decrypt, or player embedding.

## Usage

```
python3 mock_live_dash.py <dir-of-dumped-audio-segments> <port>
```

The segment dir is what `FRENCHETV_DUMP_SEGMENT=1` writes to
(`~/frenchetv-audio-dump/`, see `crates/ui-desktop/src/drm/proxy.rs`). It
needs `00000_init_*.dec.mp4` and at least one `*_media_*.dec.mp4` present —
their content is reused (with `tfdt.decode_time` patched per request) for
every `$Time$`/`$Number$` the manifest advertises.

Point mpv at `http://127.0.0.1:<port>/manifest.mpd` and watch its log for
`Invalid audio PTS` / `env_facs_q ... is invalid`:

```
mpv --no-config --idle=no --ao=null --hwdec=no --cache=yes \
    --demuxer-readahead-secs=3 --cache-pause=no --audio-buffer=2.5 \
    --audio-stream-silence=yes --demuxer-lavf-o=multiple_requests=1 \
    --loop-file=no --loop-playlist=no \
    --log-file=/tmp/mpv-repro.log --msg-level=demux=debug,ao=debug,audio=debug,all=warn \
    http://127.0.0.1:<port>/manifest.mpd
```

## Modes (env vars)

| Env var | Manifest shape | Result (90s run) |
|---|---|---|
| *(none)* | `type="dynamic"`, single sliding `<S t=X d=92160 r="15"/>`, replaced wholesale every refresh — matches what the real proxy passes through from Orange's origin verbatim | **Breaks** at ~30s (~16 segments) |
| `GROWING_TIMELINE=1` | `type="dynamic"`, stable growing multi-`<S>` list (fixed per-entry `t`, appended per tick, dropped from the front past 16) | **Breaks** even earlier, ~17s (~9 segments) — while still purely growing, before any eviction |
| `STATIC_TIMELINE=1` | `type="static"`, full 80-segment timeline embedded once, no `minimumUpdatePeriod`, manifest never refreshed | **Clean**, 0 occurrences |
| `NUMBER_BASED=1` | `type="dynamic"`, `minimumUpdatePeriod="PT2S"` (genuinely live, refreshes normally), `$Number$` + fixed `duration=` addressing, **no `<SegmentTimeline>` element at all** | **Clean**, 0 occurrences |

## Conclusion

The trigger is periodic refresh of a live MPD that uses an explicit
`SegmentTimeline`, independent of how the timeline element changes between
refreshes (sliding-single-entry vs. growing-multi-entry both break; only
removing `SegmentTimeline` in favor of `$Number$`+`duration` addressing
avoids it, while still refreshing live).

This is the basis for the proxy-side fix: rewrite the served MPD to use
`$Number$`-based `SegmentTemplate` per representation instead of passing
Orange's `SegmentTimeline`/`$Time$` structure through verbatim, translating
number → time internally before hitting the real CDN. See `proxy.rs` for
the implementation and its caveats (per-representation `duration`/`timescale`,
fallback to passthrough if a representation's segment duration isn't
provably constant).
