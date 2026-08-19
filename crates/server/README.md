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

### Para Linux, desde Windows

Un binario sirve para un solo sistema operativo: el `.exe` no corre en el
server. Esto compila el de Linux sin salir de Windows y sin instalar nada en
el server — ni Rust, ni el código fuente.

Una vez:

```
winget install zig.zig
cargo install cargo-zigbuild
rustup target add x86_64-unknown-linux-musl
```

Cada vez:

```
cargo zigbuild --release -p sway-server --target x86_64-unknown-linux-musl
```

Sale un único archivo en
`target/x86_64-unknown-linux-musl/release/sway-server`, y es lo único que hay
que copiar.

**musl y no gnu** a propósito: el binario queda estáticamente enlazado, con
SQLite adentro, así que no le importa qué versión de glibc tenga el server ni
qué distribución sea. Con `gnu`, un binario compilado contra una glibc más
nueva que la del server no arranca — y el error no dice eso, dice que no
encuentra el archivo.

`zig` acá no es el lenguaje: es su compilador de C, que sabe apuntarle a otro
sistema. Hace falta porque SQLite viaja como código C y hay que compilarlo
para el destino.

## Correr

```
./sway-server [ruta-del-config]
```

Sin argumento usa `config.toml` del directorio actual. La primera corrida
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

## Avisos de cambios

El sync lo maneja siempre el dispositivo: el server atiende y no llama a nadie
—no puede, y es a propósito: así no hace falta que los dispositivos sean
alcanzables desde internet—. Para que un cambio hecho afuera de casa no tarde
en notarse, cada dispositivo deja **una conexión abierta esperando**, y el
server contesta por ahí en cuanto su biblioteca se mueve. El que esperaba
sincroniza; al que causó el cambio no se le avisa.

Importa para quien ponga algo en el medio (un proxy TCP, un NAT): esas
conexiones están calladas casi todo el tiempo. El server manda un latido cada
45 segundos para mantenerlas vivas, así que cualquier plazo de inactividad en
el camino tiene que ser mayor a eso — el default de los Streams de Nginx Proxy
Manager son 10 minutos y alcanza de sobra.

Cuarenta y cinco segundos y no más porque el plazo que manda no es el del
proxy: es el del NAT de la operadora, que en datos móviles corta lo que está
callado entre los 30 y los 60 segundos. Que la conexión se caiga igual no es
grave —el dispositivo reconecta diciendo qué revisión conocía y el server le
contesta si se perdió algo—, pero cada caída deja al server un rato hablándole
a un muerto hasta que le falla el latido.

Un server anterior a esto corta la conexión al recibir el pedido de espera. La
app lo detecta después de tres intentos y vuelve a la pasada periódica, así que
no se rompe nada; sólo que los cambios remotos tardan como antes. Conviene
actualizar el server antes que la app.

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
ExecStart=/usr/local/bin/sway-server /var/lib/sway/config.toml
Restart=on-failure

[Install]
WantedBy=multi-user.target
```
