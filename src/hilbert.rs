#[inline]
pub(crate) fn xy_to_hilbert(base_z: u8, x: u32, y: u32) -> u64 {
    fast_hilbert::xy2h(x, y, base_z)
}

#[inline]
pub(crate) fn hilbert_to_xy(base_z: u8, id: u64) -> (u32, u32) {
    fast_hilbert::h2xy(id, base_z)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xy_round_trip() {
        for z in 0..=8 {
            let edge = (1u32 << z).saturating_sub(1);
            for (x, y) in [(0, 0), (edge, 0), (0, edge), (edge, edge)] {
                assert_eq!(hilbert_to_xy(z, xy_to_hilbert(z, x, y)), (x, y));
            }
        }
    }
}
