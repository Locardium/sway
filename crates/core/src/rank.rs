//! Ranks fraccionales (estilo LexoRank) para el orden manual de playlists,
//! carpetas y tracks dentro de una playlist.
//!
//! Por qué no un `position INTEGER` secuencial: mover un elemento renumera a
//! todos sus hermanos, así que dos dispositivos que reordenan la misma
//! playlist offline producen escrituras que se pisan entre sí — la única
//! resolución posible sería quedarse con un orden y tirar el otro entero.
//!
//! Con un rank fraccional, insertar entre dos vecinos genera un string
//! intermedio y **no toca ninguna otra fila**. Dos reordenamientos
//! concurrentes tocan filas distintas y mergean sin conflicto.
//!
//! El alfabeto son 62 caracteres ASCII en orden ascendente, así que comparar
//! los ranks como texto (colación BINARY, la default de SQLite) da el mismo
//! resultado que compararlos dígito a dígito. `ORDER BY rank` alcanza.

const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const BASE: i32 = 62;

fn digit(c: u8) -> i32 {
    ALPHABET.iter().position(|&a| a == c).unwrap_or(0) as i32
}

/// Genera un rank estrictamente entre `prev` y `next`.
/// `None` significa "sin vecino de ese lado" (principio o fin de la lista).
///
/// Nunca devuelve un string terminado en el dígito mínimo: sólo se emite un
/// dígito nuevo cuando hay lugar real entre los dos vecinos, así que siempre
/// queda espacio para insertar de nuevo a cualquiera de los dos lados.
pub fn between(prev: Option<&str>, next: Option<&str>) -> String {
    // Orden invertido (no debería pasar): degradar a "después de prev" en vez
    // de generar un rank inválido que rompa el orden en silencio.
    if let (Some(p), Some(n)) = (prev, next) {
        if p >= n {
            return between(Some(p), None);
        }
    }
    let p = prev.unwrap_or("").as_bytes();
    let n = next.unwrap_or("").as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        let pd = if i < p.len() { digit(p[i]) } else { 0 };
        // Pasado el final de `next` el hueco llega hasta el tope del alfabeto;
        // si no hay `next`, también.
        let nd = if i < n.len() { digit(n[i]) } else { BASE };
        if nd - pd > 1 {
            out.push(ALPHABET[((pd + nd) / 2) as usize]);
            break;
        }
        // Sin lugar en este dígito: copiar el de `prev` y afinar en el
        // siguiente. El string se alarga sólo lo necesario.
        out.push(if i < p.len() { p[i] } else { ALPHABET[0] });
        i += 1;
    }
    String::from_utf8(out).expect("alfabeto ASCII")
}

/// Ranks para una lista que se numera de cero (import inicial, migración).
/// Espaciados para dejar lugar entre medio sin tener que alargar strings.
pub fn initial_ranks(count: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(count);
    let mut prev: Option<String> = None;
    for _ in 0..count {
        let r = between(prev.as_deref(), None);
        prev = Some(r.clone());
        out.push(r);
    }
    out
}

/// Rank para insertar en `index` dentro de `siblings` (ya ordenados).
pub fn rank_at(siblings: &[String], index: usize) -> String {
    let index = index.min(siblings.len());
    let prev = if index == 0 { None } else { siblings.get(index - 1).map(|s| s.as_str()) };
    let next = siblings.get(index).map(|s| s.as_str());
    between(prev, next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn between_is_strictly_ordered() {
        let a = between(None, None);
        let before = between(None, Some(&a));
        let after = between(Some(&a), None);
        assert!(before < a, "{before} < {a}");
        assert!(a < after, "{a} < {after}");
        let mid = between(Some(&before), Some(&a));
        assert!(before < mid && mid < a, "{before} < {mid} < {a}");
    }

    #[test]
    fn repeated_insertion_between_two_neighbours_keeps_order() {
        // El peor caso: siempre meter en el mismo hueco. Los strings se
        // alargan, pero el orden nunca se rompe.
        let mut lo = between(None, None);
        let hi = between(Some(&lo), None);
        for _ in 0..200 {
            let mid = between(Some(&lo), Some(&hi));
            assert!(lo < mid && mid < hi, "{lo} < {mid} < {hi}");
            lo = mid;
        }
    }

    #[test]
    fn initial_ranks_are_ascending_and_unique() {
        let ranks = initial_ranks(50);
        for w in ranks.windows(2) {
            assert!(w[0] < w[1], "{} < {}", w[0], w[1]);
        }
        let mut sorted = ranks.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 50);
    }

    #[test]
    fn rank_at_places_element_at_index() {
        let siblings = initial_ranks(5);
        let first = rank_at(&siblings, 0);
        assert!(first < siblings[0]);
        let last = rank_at(&siblings, 5);
        assert!(last > siblings[4]);
        let middle = rank_at(&siblings, 2);
        assert!(siblings[1] < middle && middle < siblings[2]);
    }

    #[test]
    fn never_ends_in_min_digit() {
        // Un rank terminado en '0' no deja lugar para insertar justo antes
        // sin alargar indefinidamente; el generador no debe producirlos.
        let mut prev = between(None, None);
        for _ in 0..100 {
            assert!(!prev.ends_with('0'), "rank termina en 0: {prev}");
            prev = between(None, Some(&prev));
        }
    }

    #[test]
    fn inverted_input_degrades_to_after_prev() {
        let r = between(Some("z"), Some("a"));
        assert!(r > "z".to_string());
    }
}
