# Product

## Register

product

## Users

DJs (empezando por el propio autor) que organizan su biblioteca musical local: importan carpetas de FLAC/MP3, arman jerarquías de carpetas/playlists para sets, pre-escuchan tracks y después exportan a software DJ legacy (Rekordbox vía iTunes XML, Serato). Contexto de uso: sesiones largas de organización en desktop, típicamente de noche, pantalla única, mucho teclado y drag&drop.

## Product Purpose

Sway es un reproductor/organizador DJ multiplataforma (Tauri). Reemplaza el flujo iTunes/Explorer para curar la biblioteca: importar, taggear, ordenar manualmente playlists jerárquicas, y proyectar esa jerarquía a formatos legacy. Éxito = organizar un set de 50 tracks más rápido y más cómodo que en Rekordbox mismo, sin perder metadata.

## Brand Personality

Profundo, preciso, fluido. Interfaz oscura de cabina — negro profundo real (no gris oscuro), un solo acento azul/celeste frío que evoca "sway" (movimiento, agua). Densidad de información alta pero con jerarquía clara; el track y su metadata son los protagonistas.

## Anti-references

- Clon genérico de Spotify/streaming consumer (cards de álbumes gigantes, hero banners, "recomendados para vos").
- Planilla fría tipo Excel/foobar2000 sin jerarquía visual.
- Naranja/coral como acento (descartado explícitamente pese al reference Playcloud; del reference se toma: negro profundo, player bar flotante, panel derecho colapsable, top bar mínima).

## Design Principles

1. **La tabla es el instrumento** — la vista de tracks es donde se vive; densidad alta, orden manual sagrado, columnas configurables.
2. **Drag&drop es el verbo principal** — todo lo que se organiza se arrastra; los targets y los indicadores de drop deben ser obvios.
3. **Oscuridad de cabina** — negro profundo, contraste alto en texto primario, el acento se gana su lugar (playing, drop targets, foco).
4. **Sin ceremonia** — acciones directas: click derecho, doble click, Supr, atajos; nada de wizards ni confirmaciones innecesarias (solo para lo destructivo).
5. **Legacy-first** — nada en la UI puede romper la proyección a iTunes XML/Serato (jerarquía carpeta/playlist, orden manual).

## Accessibility & Inclusion

Contraste WCAG AA mínimo (4.5:1 texto normal) sobre fondos oscuros. Reduced motion respetado en toda animación. Targets de click ≥ 24px. Operación por teclado en tabla (selección, Supr, Ctrl+A) y modales (Enter/Escape).
