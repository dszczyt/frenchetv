#!/usr/bin/env python3
# Fully self-contained dynamic DASH server: no CDN, no our proxy, no decrypt
# path at all. Mirrors the real rewritten manifest's exact shape (SegmentTemplate
# + $Time$ + a single <S t=.. d=92160 r="15"/> that advances with wall clock,
# minimumUpdatePeriod=PT2S, timeShiftBufferDepth=PT30S) and serves a known-good
# decrypted audio segment (from the real dump) for every $Time$ value. If
# ffmpeg's dash demuxer still gets stuck around the same session-relative
# point against THIS, the bug is in ffmpeg's manifest/timeline handling, not
# in our proxy, decrypt, or network path.
import http.server
import socketserver
import sys
import time

DUMP_DIR = sys.argv[1]
PORT = int(sys.argv[2])

with open(f"{DUMP_DIR}/00000_init_livetv_france2_ctv-audio_104130_fra_104000.dec.mp4", "rb") as f:
    INIT = f.read()
with open(f"{DUMP_DIR}/00006_media_livetv_france2_ctv-audio_104130_fra_104000-85717738001083.dec.mp4", "rb") as f:
    MEDIA_TEMPLATE = bytearray(f.read())

_TFDT_IDX = MEDIA_TEMPLATE.find(b"tfdt")
assert _TFDT_IDX != -1, "template segment has no tfdt box"
_TFDT_VERSION = MEDIA_TEMPLATE[_TFDT_IDX + 4]
assert _TFDT_VERSION == 1, "expected 64-bit tfdt"
_TFDT_TIME_OFF = _TFDT_IDX + 4 + 4  # payload start + version/flags(4)


def media_for_time(t):
    # Patch tfdt.decode_time to the requested $Time$ so each segment's
    # internal PTS genuinely matches what the manifest promised for that
    # URL — a naive mock that always returns the same bytes would itself
    # produce a real PTS regression, which would test the mock, not mpv.
    buf = bytearray(MEDIA_TEMPLATE)
    buf[_TFDT_TIME_OFF:_TFDT_TIME_OFF + 8] = int(t).to_bytes(8, "big")
    return bytes(buf)


TIMESCALE = 48000
SEG_DUR = 92160  # matches real content: 1.92s per segment
START_T = 85717774404283  # arbitrary base, matches real magnitude
START_WALL = time.time()


WINDOW_SIZE = 16  # matches r="15" (16 entries) from the real manifest

# GROWING_TIMELINE=1: emit a stable, growing multi-<S> list (append one entry
# per tick, drop from the front past WINDOW_SIZE) instead of a single entry
# that slides its own t forward every refresh. Tests whether ffmpeg's dash
# demuxer only gets stuck on the *sliding-single-entry* shape specifically —
# both represent the identical stream/window, just structured differently.
GROWING_TIMELINE = __import__("os").environ.get("GROWING_TIMELINE") == "1"


def current_window():
    elapsed = time.time() - START_WALL
    segs_elapsed = int(elapsed * TIMESCALE / SEG_DUR)
    window_start_t = START_T + segs_elapsed * SEG_DUR
    return window_start_t, segs_elapsed


def segment_timeline_xml():
    t, segs_elapsed = current_window()
    if not GROWING_TIMELINE:
        return f'<S t="{t}" d="{SEG_DUR}" r="15"/>'
    # Growing variant: one <S> per segment from the start, capped to the last
    # WINDOW_SIZE entries — same segments visible, no single-entry re-slide.
    first_index = max(0, segs_elapsed - WINDOW_SIZE + 1)
    entries = []
    for i in range(first_index, segs_elapsed + 1):
        entries.append(f'<S t="{START_T + i * SEG_DUR}" d="{SEG_DUR}"/>')
    return "\n            ".join(entries)


STATIC_TIMELINE = __import__("os").environ.get("STATIC_TIMELINE") == "1"
NUMBER_BASED = __import__("os").environ.get("NUMBER_BASED") == "1"
STATIC_SEGMENTS = 80  # ~154s of content, embedded once, manifest never changes
START_NUMBER = 1


def build_mpd():
    if NUMBER_BASED:
        # $Number$-based addressing, no SegmentTimeline element at all — a
        # different, simpler ffmpeg dash-demuxer code path than the
        # SegmentTimeline-refresh-reconciliation logic tested above.
        _, segs_elapsed = current_window()
        template = (
            f'<SegmentTemplate timescale="{TIMESCALE}" duration="{SEG_DUR}" '
            f'startNumber="{START_NUMBER}" '
            f'initialization="/init.mp4" media="/media/num-$Number$.mp4"/>'
        )
        mpd_type = "dynamic"
        extra_attrs = 'minimumUpdatePeriod="PT2S" timeShiftBufferDepth="PT30S"'
    elif STATIC_TIMELINE:
        timeline = f'<S t="{START_T}" d="{SEG_DUR}" r="{STATIC_SEGMENTS - 1}"/>'
        template = (
            f'<SegmentTemplate timescale="{TIMESCALE}" '
            f'initialization="/init.mp4" media="/media/$Time$.mp4">'
            f"<SegmentTimeline>{timeline}</SegmentTimeline></SegmentTemplate>"
        )
        mpd_type = "static"
        extra_attrs = 'mediaPresentationDuration="PT200S"'
    else:
        timeline = segment_timeline_xml()
        template = (
            f'<SegmentTemplate timescale="{TIMESCALE}" '
            f'initialization="/init.mp4" media="/media/$Time$.mp4">'
            f"<SegmentTimeline>{timeline}</SegmentTimeline></SegmentTemplate>"
        )
        mpd_type = "dynamic"
        extra_attrs = 'minimumUpdatePeriod="PT2S" timeShiftBufferDepth="PT30S"'
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="{mpd_type}"
     availabilityStartTime="1970-01-01T00:00:00Z"
     {extra_attrs} maxSegmentDuration="PT2S"
     minBufferTime="PT2S" profiles="urn:mpeg:dash:profile:isoff-live:2011">
  <Period>
    <AdaptationSet mimeType="audio/mp4" segmentAlignment="true" audioSamplingRate="48000" lang="fr">
      <Representation id="audio_104130_fra=104000" bandwidth="104000" codecs="mp4a.40.2">
        {template}
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>
"""


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        if self.path == "/manifest.mpd":
            body = build_mpd().encode()
            ctype = "application/dash+xml"
        elif self.path == "/init.mp4":
            body = INIT
            ctype = "video/mp4"
        elif self.path.startswith("/media/num-"):
            n = int(self.path[len("/media/num-"):].rsplit(".mp4", 1)[0])
            t = START_T + (n - START_NUMBER) * SEG_DUR
            body = media_for_time(t)
            ctype = "video/mp4"
        elif self.path.startswith("/media/"):
            t_str = self.path[len("/media/"):].rsplit(".mp4", 1)[0]
            body = media_for_time(int(t_str))
            ctype = "video/mp4"
        else:
            self.send_response(404)
            self.end_headers()
            return

        self.send_response(200, "OK")
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "keep-alive")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format, *_args):
        pass


class ThreadingHTTPServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


srv = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
srv.serve_forever()
