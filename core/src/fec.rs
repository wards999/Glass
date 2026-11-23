//! Forward error correction (M4, ADR 0020): a systematic Reed–Solomon
//! erasure code over GF(2^8), with a Cauchy matrix.
//!
//! A group of `k` equal-length data shards gets `m` parity shards. The data
//! shards are sent unchanged; any `e ≤ m` lost data shards can be rebuilt
//! from any `e` parity shards of the same group, with the data shards that
//! did arrive. Parity row `j` over data shard `i` uses the coefficient
//! `1 / (x_j + y_i)`, with `x_j = k + j` and `y_i = i` (all distinct, so
//! every square submatrix is invertible). `k + m ≤ 256`.
//!
//! Encoding costs one table lookup and XOR per parity byte per data shard;
//! decoding only runs when something was lost.

use std::sync::OnceLock;

/// The field's polynomial: x^8 + x^4 + x^3 + x^2 + 1.
const POLY: u16 = 0x11D;
/// At most this many shards (data + parity) in one group.
pub const MAX_SHARDS: usize = 256;

struct Tables {
    exp: [u8; 512],
    log: [u8; 256],
    /// `mul[a][b] = a·b`: 64 KB, one lookup per byte.
    mul: Box<[[u8; 256]; 256]>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for (i, e) in exp.iter_mut().enumerate().take(255) {
            *e = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= POLY;
            }
        }
        for i in 255..512 {
            exp[i] = exp[i - 255];
        }
        let mut mul = Box::new([[0u8; 256]; 256]);
        for a in 1..256 {
            for b in 1..256 {
                mul[a][b] = exp[log[a] as usize + log[b] as usize];
            }
        }
        Tables { exp, log, mul }
    })
}

fn mul(a: u8, b: u8) -> u8 {
    tables().mul[a as usize][b as usize]
}

fn inv(a: u8) -> u8 {
    debug_assert!(a != 0);
    let t = tables();
    t.exp[255 - t.log[a as usize] as usize]
}

/// The coefficient of data shard `i` in parity row `j`, for `k` data shards.
fn coef(k: usize, j: usize, i: usize) -> u8 {
    inv(((k + j) as u8) ^ (i as u8))
}

/// `dst ^= c · src`, byte by byte.
fn mul_add(dst: &mut [u8], src: &[u8], c: u8) {
    match c {
        0 => {}
        1 => dst.iter_mut().zip(src).for_each(|(d, s)| *d ^= *s),
        _ => {
            let row = &tables().mul[c as usize];
            dst.iter_mut()
                .zip(src)
                .for_each(|(d, s)| *d ^= row[*s as usize]);
        }
    }
}

/// Fills `parity` (each as long as the data shards) for the `data` shards.
pub fn encode(data: &[&[u8]], parity: &mut [&mut [u8]]) {
    let k = data.len();
    assert!(k + parity.len() <= MAX_SHARDS, "too many shards");
    for (j, p) in parity.iter_mut().enumerate() {
        p.fill(0);
        for (i, d) in data.iter().enumerate() {
            mul_add(p, d, coef(k, j, i));
        }
    }
}

/// `encode` for one interleaved group of a frame laid out as whole shards
/// in `src` (shard `i` at `i * shard`): the group's data shards are `first`,
/// `first + step`, … (`k` of them); its `m` parity shards go into `parity`,
/// one after another. No allocation: it runs for every frame.
pub fn encode_strided(
    src: &[u8],
    shard: usize,
    (first, step, k): (usize, usize, usize),
    parity: &mut [u8],
    m: usize,
) {
    assert!(k + m <= MAX_SHARDS, "too many shards");
    for j in 0..m {
        let p = &mut parity[j * shard..(j + 1) * shard];
        p.fill(0);
        for t in 0..k {
            let i = first + t * step;
            mul_add(p, &src[i * shard..(i + 1) * shard], coef(k, j, t));
        }
    }
}

/// Inverts the square matrix `a` (row-major, `n x n`) in place. False if
/// it's singular (never, for a Cauchy submatrix).
fn invert(a: &mut [u8], n: usize) -> bool {
    let mut b = vec![0u8; n * n];
    for i in 0..n {
        b[i * n + i] = 1;
    }
    for col in 0..n {
        let Some(piv) = (col..n).find(|&r| a[r * n + col] != 0) else {
            return false;
        };
        if piv != col {
            for c in 0..n {
                a.swap(piv * n + c, col * n + c);
                b.swap(piv * n + c, col * n + c);
            }
        }
        let f = inv(a[col * n + col]);
        for c in 0..n {
            a[col * n + c] = mul(a[col * n + c], f);
            b[col * n + c] = mul(b[col * n + c], f);
        }
        for r in 0..n {
            let g = a[r * n + col];
            if r == col || g == 0 {
                continue;
            }
            for c in 0..n {
                a[r * n + c] ^= mul(g, a[col * n + c]);
                b[r * n + c] ^= mul(g, b[col * n + c]);
            }
        }
    }
    a.copy_from_slice(&b);
    true
}

/// Rebuilds missing data shards. `data`: the group's `k` data shards (the
/// missing ones' contents are overwritten); `present[i]`: shard `i` arrived;
/// `parity`: the parity shards that arrived, as `(row j, bytes)`. True if
/// every data shard is there afterwards.
pub fn reconstruct(data: &mut [&mut [u8]], present: &[bool], parity: &[(usize, &[u8])]) -> bool {
    let k = data.len();
    let missing: Vec<usize> = (0..k).filter(|&i| !present[i]).collect();
    let e = missing.len();
    if e == 0 {
        return true;
    }
    if parity.len() < e || k + parity.iter().map(|p| p.0 + 1).max().unwrap_or(0) > MAX_SHARDS {
        return false;
    }
    let len = data.iter().map(|d| d.len()).max().unwrap_or(0);
    let rows = &parity[..e];
    // s_r = parity_j minus what the data that arrived contributes: left is
    // the missing shards' part, sum over t of coef(j, missing_t) · d_t.
    let mut s: Vec<Vec<u8>> = rows.iter().map(|(_, p)| p[..len].to_vec()).collect();
    for (r, (j, _)) in rows.iter().enumerate() {
        for (i, d) in data.iter().enumerate() {
            if present[i] {
                mul_add(&mut s[r], d, coef(k, *j, i));
            }
        }
    }
    let mut m = vec![0u8; e * e];
    for (r, (j, _)) in rows.iter().enumerate() {
        for (t, &i) in missing.iter().enumerate() {
            m[r * e + t] = coef(k, *j, i);
        }
    }
    if !invert(&mut m, e) {
        return false;
    }
    for (t, &i) in missing.iter().enumerate() {
        let d = &mut data[i];
        d.fill(0);
        for (r, sr) in s.iter().enumerate() {
            mul_add(d, sr, m[t * e + r]);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shards(k: usize, len: usize, seed: u32) -> Vec<Vec<u8>> {
        let mut x = seed.wrapping_mul(2_654_435_761).max(1);
        (0..k)
            .map(|_| {
                (0..len)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 17;
                        x ^= x << 5;
                        x as u8
                    })
                    .collect()
            })
            .collect()
    }

    fn parity_of(data: &[Vec<u8>], m: usize) -> Vec<Vec<u8>> {
        let len = data[0].len();
        let mut p = vec![vec![0u8; len]; m];
        let d: Vec<&[u8]> = data.iter().map(|v| v.as_slice()).collect();
        let mut pr: Vec<&mut [u8]> = p.iter_mut().map(|v| v.as_mut_slice()).collect();
        encode(&d, &mut pr);
        p
    }

    /// Loses `lost` data shards and the parity rows not in `keep`; rebuilds.
    fn recover(data: &[Vec<u8>], parity: &[Vec<u8>], lost: &[usize], keep: &[usize]) -> bool {
        let mut got: Vec<Vec<u8>> = data.to_vec();
        let mut present = vec![true; data.len()];
        for &i in lost {
            present[i] = false;
            got[i].fill(0xAA); // garbage where it's missing
        }
        let par: Vec<(usize, &[u8])> = keep.iter().map(|&j| (j, parity[j].as_slice())).collect();
        let mut refs: Vec<&mut [u8]> = got.iter_mut().map(|v| v.as_mut_slice()).collect();
        let ok = reconstruct(&mut refs, &present, &par);
        ok && got == data
    }

    #[test]
    fn field_arithmetic() {
        for a in 1..=255u8 {
            assert_eq!(mul(a, inv(a)), 1, "{a}");
            assert_eq!(mul(a, 1), a);
            assert_eq!(mul(a, 0), 0);
        }
        // Distributive, on a few values.
        for (a, b, c) in [(3u8, 7u8, 200u8), (255, 254, 1), (17, 99, 128)] {
            assert_eq!(mul(a, b ^ c), mul(a, b) ^ mul(a, c));
        }
    }

    #[test]
    fn rebuilds_any_combination_up_to_the_parity() {
        let (k, m) = (10, 4);
        let data = shards(k, 1352, 7);
        let parity = parity_of(&data, m);
        // Every pair of lost data shards, from every pair of parity rows.
        for a in 0..k {
            for b in a + 1..k {
                for (p, q) in [(0, 1), (1, 3), (2, 0), (3, 2)] {
                    assert!(
                        recover(&data, &parity, &[a, b], &[p, q]),
                        "{a},{b} via {p},{q}"
                    );
                }
            }
        }
        // Four lost: all four parity rows needed.
        assert!(recover(&data, &parity, &[0, 3, 5, 9], &[0, 1, 2, 3]));
        // Five lost: impossible.
        assert!(!recover(&data, &parity, &[0, 1, 2, 3, 4], &[0, 1, 2, 3]));
        // Fewer parity rows than lost shards: impossible.
        assert!(!recover(&data, &parity, &[0, 1], &[2]));
        // Nothing lost: nothing to do.
        assert!(recover(&data, &parity, &[], &[]));
    }

    #[test]
    fn strided_matches_plain() {
        // Three interleaved groups over 10 shards: group 1 is shards 1, 4, 7.
        let all = shards(10, 32, 5);
        let flat: Vec<u8> = all.concat();
        let mut par = vec![0u8; 2 * 32];
        encode_strided(&flat, 32, (1, 3, 3), &mut par, 2);
        let group = vec![all[1].clone(), all[4].clone(), all[7].clone()];
        assert_eq!(par, parity_of(&group, 2).concat());
    }

    #[test]
    fn large_and_tiny_groups() {
        let data = shards(200, 64, 3);
        let parity = parity_of(&data, 40);
        let lost: Vec<usize> = (0..200).step_by(5).collect(); // 40 lost
        let keep: Vec<usize> = (0..40).rev().collect();
        assert!(recover(&data, &parity, &lost, &keep));
        let one = shards(1, 10, 9);
        let p = parity_of(&one, 1);
        assert!(recover(&one, &p, &[0], &[0]));
    }
}
