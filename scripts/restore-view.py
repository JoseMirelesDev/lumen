#!/usr/bin/env python3
# Restore min-view experiment: re-enable particles Image and seats;
# SceneProp loop filtered to static-only (fire moves to RAM frames).
p = 'apps/lumen-slint/ui/voice-view.slint'
s = open(p).read()

# 1. Restore particles Image (remove comment wrapper)
old_img_comment = """    /* MIN-VIEW: particles commented
    if root.connected && !root.reduced-motion : Image {
        source: root.fire-frame;
        x: 0px;
        y: 0px;
        width: 100%;
        height: 100%;
        image-rendering: pixelated;
        image-fit: fill;
    }
    */"""
assert old_img_comment in s, "particles comment block not found"
s = s.replace(old_img_comment, """    if root.connected && !root.reduced-motion : Image {
        source: root.fire-frame;
        x: 0px;
        y: 0px;
        width: 100%;
        height: 100%;
        image-rendering: pixelated;
        image-fit: fill;
    }""")

# 2. Restore seats (remove comment wrapper, drop the extra brace we added)
old_seats_comment = """    /* MIN-VIEW: seats commented
    if root.connected : Seat {
        x: root.seat-x(0, root.total, root.width, root.height);
        y: root.seat-y(0, root.total, root.width, root.height);
        transform-scale-x: root.seat-scale(0, root.total);
        transform-scale-y: root.seat-scale(0, root.total);
        username: root.local-user;
        skin: root.local-skin;
        talking: root.local-speaking;
        muted: root.local-muted;
        deafened: root.local-deafened;
        reduced-motion: root.reduced-motion;
        time: root.slow-time;
    }
    for p[i] in root.peers: Seat {
        x: root.seat-x(i + 1, root.total, root.width, root.height);
        y: root.seat-y(i + 1, root.total, root.width, root.height);
        transform-scale-x: root.seat-scale(i + 1, root.total);
        transform-scale-y: root.seat-scale(i + 1, root.total);
        username: p.username;
        skin: p.skin;
        talking: p.speaking;
        muted: p.muted;
        deafened: false;
        reduced-motion: root.reduced-motion;
        time: root.slow-time;
    }
}
    */
}"""
new_seats = """    if root.connected : Seat {
        x: root.seat-x(0, root.total, root.width, root.height);
        y: root.seat-y(0, root.total, root.width, root.height);
        transform-scale-x: root.seat-scale(0, root.total);
        transform-scale-y: root.seat-scale(0, root.total);
        username: root.local-user;
        skin: root.local-skin;
        talking: root.local-speaking;
        muted: root.local-muted;
        deafened: root.local-deafened;
        reduced-motion: root.reduced-motion;
        time: root.slow-time;
    }
    for p[i] in root.peers: Seat {
        x: root.seat-x(i + 1, root.total, root.width, root.height);
        y: root.seat-y(i + 1, root.total, root.width, root.height);
        transform-scale-x: root.seat-scale(i + 1, root.total);
        transform-scale-y: root.seat-scale(i + 1, root.total);
        username: p.username;
        skin: p.skin;
        talking: p.speaking;
        muted: p.muted;
        deafened: false;
        reduced-motion: root.reduced-motion;
        time: root.slow-time;
    }
}"""
assert old_seats_comment in s, "seats comment block not found"
s = s.replace(old_seats_comment, new_seats)

# 3. SceneProp loop: filter to static-only (anim-ms == 0); fire goes to RAM frames
old_fire = """    /* MIN-VIEW: fire commented
    for e in SceneManifest.campfire: SceneProp {
        el: e;
        scale: root.scene-scale;
        running: root.connected;
        reduced-motion: root.reduced-motion;
        time: root.slow-time;
        visible: root.e-visible(e);
    }
    */"""
new_fire = """    // Elementos ESTÁTICOS de la escena (sin animación) quedan en Slint.
    // El fuego (brazier + torches, anim-ms > 0) se renderiza en los frames RAM
    // de particles.rs — fuera del árbol, sin re-evaluación por tick.
    for e in SceneManifest.campfire: if e.anim-ms == 0 : SceneProp {
        el: e;
        scale: root.scene-scale;
        running: root.connected;
        reduced-motion: root.reduced-motion;
        time: root.slow-time;
        visible: root.e-visible(e);
    }"""
assert old_fire in s, "fire comment block not found"
s = s.replace(old_fire, new_fire)

# 4. de-export components now used again (Seat, SceneProp); keep Particle* exported
#    (ParticleField/ParticleLayer are no longer instantiated in the tree)
s = s.replace("export component Seat inherits Rectangle {", "component Seat inherits Rectangle {", 1)
s = s.replace("export component SceneProp inherits Rectangle {", "component SceneProp inherits Rectangle {", 1)

open(p, 'w').write(s)
print("done: particles+seats restored, SceneProp static-only, fire to RAM")
