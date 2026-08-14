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
pair_token = "..."
```

- `music_dir` es lo que crece: apuntalo al disco grande, no al del sistema.
- `pair_token` es lo que hay que poner en la app para vincular un dispositivo.
  Reemplaza al código de seis dígitos, porque acá no hay pantalla donde
  compararlo. **Tratalo como una contraseña.** Cambiarlo no desvincula lo que ya
  está vinculado: las claves ya están guardadas.

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
