#!/usr/bin/env python3
"""Analyze LUMEN_VOICE_DIAG traces from a real two-machine call.

Run the app on both PCs with LUMEN_VOICE_DIAG=1, talk for ~30 s, then analyze
the two <tmp>/lumen-voice-diag-<pid>.jsonl files:

  python3 scripts/voice-diag-analyze.py file1.jsonl [file2.jsonl]

Reported signals:
  - send : pacing (should be ~20 ms/frame) and RTP timestamp progression
  - recv : arrival pacing, sequence gaps (packet loss), jitter-buffer level
  - mix  : NetEQ concealment (expand) rate — the direct "robotic" indicator
"""
import json
import statistics
import sys


def load(path):
    rows = []
    for line in open(path):
        line = line.strip()
        if line:
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return rows


def q(v, p):
    return statistics.quantiles(sorted(v), n=20)[int(p / 5) - 1] if len(v) >= 20 else (max(v) if v else 0)


def analyze(path):
    rows = load(path)
    if not rows:
        print(f"{path}: empty")
        return
    send = [r for r in rows if r["ev"] == "send"]
    recv = [r for r in rows if r["ev"] == "recv"]
    mix = [r for r in rows if r["ev"] == "mix"]
    print(f"=== {path} ({len(send)} send, {len(recv)} recv, {len(mix)} mix ticks) ===")

    if send:
        since = [r["since_ms"] for r in send[1:]]
        ts_d = [b["ts"] - a["ts"] for a, b in zip(send, send[1:])]
        lens = [r["len"] for r in send]
        print(f"  SEND: {len(send)} pkts; pacing med {q(since,50)}ms p90 {q(since,90)}ms "
              f"max {max(since)}ms (expected ~20); ts_delta med {q(ts_d,50)} (expected 960); "
              f"payload len med {q(lens,50)}B")
        bursty = sum(1 for s in since if s > 60) / max(len(since), 1)
        if bursty > 0.1:
            print(f"    ! send pacing BURSTY: {bursty:.0%} of frames >60 ms apart")

    if recv:
        since = [r["since_ms"] for r in recv[1:]]
        gaps = [r["gap"] for r in recv]
        total_gap = sum(gaps)
        buf = [r["buffer_ms"] for r in recv]
        print(f"  RECV: {len(recv)} pkts; arrival med {q(since,50)}ms p90 {q(since,90)}ms max {max(since)}ms; "
              f"seq gaps total {total_gap} ({total_gap/max(len(recv),1):.1%} loss-equiv); "
              f"buffer med {q(buf,50)}ms max {max(buf)}ms")
        if total_gap > 0:
            print(f"    ! {total_gap} MISSING packets (loss or reorder)")
        if since and max(since) > 200:
            print(f"    ! arrival jitter high: max {max(since)}ms between packets")

    if mix:
        total = sum(r["expand"] + r["normal"] + r["cng"] + r["other"] for r in mix)
        expand = sum(r["expand"] for r in mix)
        normal = sum(r["normal"] for r in mix)
        pps = [r["pps"] for r in mix]
        buf = [r["buffer_ms"] for r in mix]
        er = expand / max(total, 1)
        print(f"  MIX: frames normal {normal} expand {expand} ({er:.1%}) cng {sum(r['cng'] for r in mix)}; "
              f"pps med {q(pps,50)}; buffer med {q(buf,50)}ms max {max(buf)}ms")
        if er > 0.02:
            print(f"    ! HIGH concealment {er:.1%}: NetEQ is hiding loss -> ROBOTIC audio")
        elif er > 0:
            print(f"    minor concealment {er:.1%} (normal)")
        else:
            print("    no concealment (clean receive)")


if __name__ == "__main__":
    for p in sys.argv[1:]:
        analyze(p)
