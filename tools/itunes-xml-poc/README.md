# Fase 0 — PoC generador iTunes XML

Genera un `iTunes Music Library.xml` desde una carpeta de audio, para **validar que Rekordbox y Serato lo importan** (incluyendo FLAC) antes de construir la app entera.

## Uso

```bash
cd tools/itunes-xml-poc
pnpm install          # solo la primera vez (instala music-metadata para leer tags)
node generate.mjs "C:\Users\User\Music" "iTunes Music Library.xml"
```

- Arg 1: carpeta raíz de tu música (escanea recursivo).
- Arg 2 (opcional): archivo de salida. Default: `iTunes Music Library.xml` en el cwd.

Cada **subcarpeta de primer nivel** se convierte en una playlist, más una playlist maestra `Library` con todo.

## Cómo probar el import (no afecta tu Collection actual)

El XML se carga en un **nodo aparte del sidebar**, separado de tu Collection. Tus playlists/cues actuales quedan intactos.

**Rekordbox:**
1. Preferencias → Advanced → Database → **iTunes**.
2. Marcar "Enable iTunes library sync" y apuntar al `iTunes Music Library.xml` generado.
3. Aparece un árbol **iTunes** en el sidebar → verificás tracks, playlists, orden, y que los **FLAC reproducen**.

**Serato:**
1. En el panel de la izquierda, expandir **iTunes** (Serato lee la librería iTunes automáticamente si el XML está en la ubicación estándar, o configurás la ruta).
2. Verificar tracks/playlists y reproducción de FLAC.

## Qué estamos validando

- [ ] Rekordbox importa el XML y muestra tracks + playlists en el orden correcto.
- [ ] Los **FLAC** reproducen en Rekordbox.
- [ ] Serato importa el XML y muestra lo mismo.
- [ ] Los FLAC reproducen en Serato.
- [ ] Rutas con espacios/caracteres especiales resuelven bien (no "file not found").

Si algo falla, anotamos el quirk del dialecto acá y ajustamos el generador. Este código es **throwaway** (PoC): la versión de producción va en Rust dentro del core (Fase 2).

## Notas

- Formato de `<Location>`: `file://localhost/C:/...` con cada segmento percent-codificado (igual que iTunes en Windows).
- `Persistent ID` se deriva del path (estable entre corridas).
- Si `music-metadata` no está instalado, cae al nombre de archivo como título (el PoC igual corre).
