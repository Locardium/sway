# sway-server

Server de archivo y sync de Sway. Guarda lo que le mandan los dispositivos y se
lo devuelve cuando lo piden. **No importa música por su cuenta y no tiene
interfaz**: todo lo que tiene, se lo mandó alguien.

Para qué sirve, en dos casos concretos:

- **Sincronizar fuera de casa.** El descubrimiento por mDNS sólo ve la red
  local, y dos dispositivos en redes distintas tampoco se pueden llamar entre
  sí: los dos están detrás de un NAT. Contra un server con dirección pública,
  en cambio, los dos marcan hacia afuera. Y como el server tiene todo, ninguno
  necesita que el otro esté prendido.
- **Recuperar.** Si se pierde la biblioteca de todos los dispositivos, acá están
  los archivos y la organización.

## Compilar

```
cargo build --release -p sway-server
```

El binario queda en `target/release/sway-server` (`.exe` en Windows). No
depende de nada del sistema: ni entorno gráfico, ni webview, ni SQLite
instalado.

## Correr

```
./sway-server [ruta-del-config]
```

Sin argumento usa `sway-server.toml` del directorio actual. La primera corrida
escribe ese archivo con un token nuevo y termina, para que lo revises antes de
abrir el puerto:

```toml
listen = "0.0.0.0:7420"
name = "Sway Server"
data_dir = "data"
music_dir = "music"
retention_days = 90
pair_token = "..."
```

- `music_dir` es lo que crece: apuntalo al disco grande, no al del sistema.
  **En Windows la barra invertida es un escape en TOML**: se pone `'D:\Musica'`
  con comillas simples, o `"D:/Musica"` con barras normales.
- `retention_days` es cuántos días sobrevive en la papelera del server un
  archivo borrado. Un borrado viaja: lo borrás en un dispositivo y desaparece
  de todos, server incluido. Esto es lo único que hace que se pueda rescatar
  después. En `0` se destruye en el acto — espejo exacto de tus dispositivos,
  sin red debajo.
- `pair_token` es lo que hay que poner en la app para vincular un dispositivo.
  Reemplaza al código de seis dígitos, porque acá no hay pantalla donde
  compararlo. **Tratalo como una contraseña.** Cambiarlo no desvincula lo que ya
  está vinculado: las claves ya están guardadas. También se puede pasar por
  la variable de entorno `SWAY_SERVER_TOKEN`, que pisa la del archivo: en un
  despliegue conviene que el secreto no viva en un archivo que puede terminar
  en un repo.

## Qué guarda

Todo. El server no elige: cada dispositivo le manda lo que tiene y se lleva lo
que le falta. La misma canción mandada por tres dispositivos ocupa **una sola
vez** — los archivos se identifican por su contenido, no por su nombre.

Por eso su configuración de sync no se puede editar desde la app: un archivo
con agujeros no es un archivo, y un server en "solo envía" no te devuelve nada
el día que lo necesitás.

## Seguridad

- Todo el tráfico va por un canal cifrado (Noise XX sobre TCP, el mismo que usan
  los dispositivos entre sí). No hace falta poner nada adelante ni montar TLS.
- Un dispositivo con una clave distinta a la que el server ya tenía para ese uid
  **se rechaza y queda registrado**, tenga el token que tenga. Volver a
  vincularlo requiere desvincularlo primero, a mano.
- El puerto tiene que llegar desde afuera: reenvío en el router o una IP
  pública. Es lo único que hay que exponer.

## systemd

```ini
[Unit]
Description=Sway sync server
After=network-online.target

[Service]
User=sway
WorkingDirectory=/var/lib/sway
ExecStart=/usr/local/bin/sway-server /var/lib/sway/sway-server.toml
Restart=on-failure

[Install]
WantedBy=multi-user.target
```
