//! Server de archivo y sync de Sway.
//!
//! El binario es un envoltorio finito sobre esto (leer la config, abrir la
//! base, escuchar). Está partido así para que los tests puedan levantar un
//! server de verdad, con su socket y su base, y hablarle por el protocolo real
//! en vez de afirmar cosas sobre funciones sueltas.

pub mod config;
pub mod host;
pub mod serve;
