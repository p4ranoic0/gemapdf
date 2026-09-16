//! Matriz de transformación PDF.
//!
//! **Copia deliberada** de `crates/gema-compress/src/geometry.rs`. Los dos
//! crates la necesitan y ninguno puede depender del otro. La matemática está
//! fijada por la especificación del PDF y no va a cambiar, pero la
//! representación, la precisión o un arreglo aplicado de un solo lado sí
//! pueden divergir: **cualquier cambio acá se replica allá, y al revés.**

/// Matriz de transformación PDF `[a b c d e f]`, que representa
/// ```text
/// | a b 0 |
/// | c d 0 |
/// | e f 1 |
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Matrix {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Matrix {
    pub(crate) const IDENTITY: Matrix = Matrix {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    /// Multiplicación de matrices PDF: `self × other` (self a la izquierda).
    ///
    /// Con la convención de vectores fila `[x y 1] · M`, aplicar primero `self`
    /// y luego `other` equivale a `self × other`. El operador `cm` **pre-**
    /// multiplica el CTM actual: `nuevo_ctm = cm × ctm_actual`, es decir
    /// `cm_matrix.mul(&ctm)`.
    pub(crate) fn mul(&self, other: &Matrix) -> Matrix {
        Matrix {
            a: self.a * other.a + self.b * other.c,
            b: self.a * other.b + self.b * other.d,
            c: self.c * other.a + self.d * other.c,
            d: self.c * other.b + self.d * other.d,
            e: self.e * other.a + self.f * other.c + other.e,
            f: self.e * other.b + self.f * other.d + other.f,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_neutral() {
        let m = Matrix {
            a: 2.0,
            b: 0.0,
            c: 0.0,
            d: 3.0,
            e: 5.0,
            f: 7.0,
        };
        assert_eq!(m.mul(&Matrix::IDENTITY), m);
        assert_eq!(Matrix::IDENTITY.mul(&m), m);
    }

    #[test]
    fn translation_composes() {
        let t1 = Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 10.0,
            f: 0.0,
        };
        let t2 = Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 5.0,
        };
        let r = t1.mul(&t2);
        assert_eq!((r.e, r.f), (10.0, 5.0));
    }
}
