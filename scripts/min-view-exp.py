#!/usr/bin/env python3
# MIN-VIEW experiment: comment fire (SceneProp), particles (Image), and seats.
import re

p = 'apps/lumen-slint/ui/voice-view.slint'
s = open(p).read()

# 1. SceneProp loop (fire)
old_for = """    for e in SceneManifest.campfire: SceneProp {
        el: e;
        scale: root.scene-scale;
        running: root.connected;
        reduced-motion: root.reduced-motion;
        time: root.slow-time;
        visible: root.e-visible(e);
    }"""
assert old_for in s, "SceneProp loop not found"
s = s.replace(old_for, "    /* MIN-VIEW: fire commented\n" + old_for + "\n    */")

# 2. particles Image
old_img = """    if root.connected && !root.reduced-motion : Image {
        source: root.fire-frame;
        x: 0px;
        y: 0px;
        width: 100%;
        height: 100%;
        image-rendering: pixelated;
        image-fit: fill;
    }"""
assert old_img in s, "particles Image not found"
s = s.replace(old_img, "    /* MIN-VIEW: particles commented\n" + old_img + "\n    */")

# 3. local Seat block: from "if root.connected : Seat {" to the closing of the peer loop
# Locate the two seat blocks
seat_start = s.index("    if root.connected : Seat {")
peer_start = s.index("    for p[i] in root.peers: Seat {")
# The local seat block ends just before the peer loop (its closing "    }\n")
# We'll comment from seat_start to the end of the peer loop.
# Find the end of peer loop: it's "    }\n}" — the first "    }\n" after peer_start closes the loop
# Actually structure: local seat { ... } then peer loop { ... } then "}\n}" closes component
# Find the closing of the peer loop: search for the pattern after peer_start
after_peer = s[peer_start:]
# the peer loop body ends with "        time: root.slow-time;\n    }\n}\n"
loop_end_marker = "        time: root.slow-time;\n    }\n}\n"
idx = after_peer.index(loop_end_marker) + len(loop_end_marker)
# comment everything from seat_start to that point
s = s[:seat_start] + "    /* MIN-VIEW: seats commented\n" + s[seat_start:peer_start + idx] + "    */" + s[peer_start + idx:]

# 4. export unused components (they are referenced only inside comments now)
for comp in ["Seat", "SceneProp", "ParticleField", "ParticleLayer"]:
    s = s.replace(f"component {comp} inherits Rectangle {{", f"export component {comp} inherits Rectangle {{", 1)

open(p, 'w').write(s)
print("done: fire, particles, seats commented; components exported")
