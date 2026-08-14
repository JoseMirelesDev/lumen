# Brief para el designer: escena de fogata de alto rendimiento

## Contexto del producto

Lumen es un cliente de escritorio estilo Discord (Rust + Slint, renderer Skia sobre OpenGL ES). El voice chat tiene una "escena de fogata" pixel-art: una hoguera con 4 frames animados, brasas/luciérnagas ambientales, asientos con personajes, HUD. Estética: **pixel art retro de juego** (sprites de 16px, paleta limitada, estilo 16-bit).

## El problema de rendimiento (medido, no especulado)

Hardware de referencia: i5-4590 (Haswell 2014), iGPU Intel HD 4600, Mesa 25.2.8. Los números son % de 1 núcleo (÷4 = % del total).

- **UI idle (chat normal): ~0.5% de un núcleo** — el objetivo es mantener esto
- **Voice view mínimo (solo fondo + HUD, nada animado): ~0.55%** — la UI sin animación ya es idle
- **Cada animación procedural agrega ~3.2%**: el fuego con 4 frames rotando + partículas a 12.5Hz llevó la UI de 0.55% a 3.8%

**Por qué es caro (límite técnico del framework):** Slint re-renderiza el árbol de la UI en cada invalidación. Cada frame de animación que rota fuerza traversal + allocs + presentación GL. Ya optimizamos con pre-render en RAM (frames generados una vez, se rota un solo Image), pero **la frecuencia de rotación es el costo**: 12.5Hz = 3.2% extra, y es proporcional.

## Restricciones duras

1. **NO animación procedural a alta frecuencia** (nada que cambie más de ~2-5 veces/seg). Cada rotación de frame cuesta CPU.
2. **NO partículas animadas** (34 sprites moviéndose = el costo más alto, ya medido).
3. **NO reducir calidad visual percibida del "feeling"**: tiene que seguir sintiendo a juego pixel-art cálido y vivo.
4. **NO tocar el pipeline de audio** (es un slice aparte).
5. **Debe funcionar en este hardware viejo** (iGPU integrada, sin aceleración fuerte).
6. El resto de la escena (asientos, HUD, fondo con gradiente) ya es barato — se mantiene.

## Lo que ya descartamos

- Fuego a 5fps rotando (funciona pero el usuario lo rechaza: quiere más vida)
- Partículas en cualquier forma (todas cuestan)
- Shaders GPU / overlay GL (rompen el partial rendering de Slint, +14% fijo)
- cache-rendering-hint (rompe gradientes en este renderer)

## Lo que necesito de vos

Proponé **2-4 alternativas de dirección visual** para la "hoguera viva" que:

- Den el feeling de fuego/juego pixel-art **sin animación por-frame**
- Sean implementables con elementos estáticos o cambios de estado MUY infrecuentes (≤2-4 veces/seg, o puramente estáticos)
- Expliquen QUÉ se ve (composición, sprites, efectos de luz) y POR QUÉ funciona psicológicamente (por qué "se siente vivo" aunque no se anime)
- Para cada una: qué elementos visuales concretos se necesitan (sprites estáticos, gradientes, glows, capas), y el costo estimado

### Ideas que podrían funcionar (explorá y mejoralas, no te limites)

1. **Fuego "respirante" por pulsos de luz** — una sola textura de fuego estática + el GLOW alrededor que late muy lentamente (1-2 veces/seg, un solo elemento con opacity animada, no sprites). El glow pulsante da vida sin mover el fuego.
2. **Humo/brasas estáticas + luz parpadeante** — brasas y chispas DIBUJADAS (no animadas) en posiciones fijas, con un parpadeo global de luz muy lento.
3. **Composición de capas con movimiento ultra-lento** — 2-3 capas de "fuego" semi-transparentes que se desplazan 1-2px muy lentamente (o con cambios de opacidad en cascada), simulando llamas sin frames.
4. **Fuego estático + eventos discretos** — el fuego es una imagen fija rica, y solo cambia de estado en eventos reales (alguien habla → destello, se une alguien → chispa). El movimiento queda asociado a acción, no a loop continuo.
5. **Ilusión de pixel-art vivo por composición** — sombras de personajes que oscilan 1px (muy lento), luz ambiental que cambia de temperatura de color lentamente, sin mover sprites.

### Formato de respuesta

Para cada propuesta:
- **Nombre** y descripción de 1-2 líneas
- **Qué se ve** (composición visual concreta)
- **Elementos técnicos** (qué sprites/glows/capas, cuántos items de UI, frecuencia de cambio)
- **Costo estimado** (bajo/medio, y si es estático o ≤2-4Hz)
- **Por qué funciona** (psicología: por qué da el feeling de fuego vivo)

Elegí la que recomendarías como #1 y justificá por qué.
