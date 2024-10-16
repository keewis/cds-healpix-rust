use std::cmp::Ordering;
use std::{iter::Map, slice::Iter, vec::IntoIter};

use super::super::{
  super::skymap::{SkyMap, SkyMapValue},
  Mom, ZUniqHashT,
};

/// Implementation of a MOM in an ordered vector of `(zuniq, values)` tuples.
pub struct MomVecImpl<Z, V>
where
  Z: ZUniqHashT,
  V: SkyMapValue,
{
  depth: u8,
  entries: Vec<(Z, V)>,
}
impl<'a, Z, V> Mom<'a> for MomVecImpl<Z, V>
where
  Z: ZUniqHashT,
  V: 'a + SkyMapValue,
{
  type ZUniqHType = Z;
  type ValueType = V;
  type ZuniqIt = Map<Iter<'a, (Z, V)>, fn(&'a (Z, V)) -> Z>;
  type ValuesIt = Map<Iter<'a, (Z, V)>, fn(&'a (Z, V)) -> &'a V>;
  type EntriesIt = Map<Iter<'a, (Z, V)>, fn(&'a (Z, V)) -> (Z, &'a V)>;
  type OwnedEntriesIt = IntoIter<(Z, V)>;

  fn depth_max(&self) -> u8 {
    self.depth
  }

  fn len(&self) -> usize {
    self.entries.len()
  }

  fn get_cell_containing_unsafe(
    &'a self,
    hash_at_depth_max: Self::ZUniqHType,
  ) -> Option<(Self::ZUniqHType, &'a Self::ValueType)> {
    match self
      .entries
      .binary_search_by(|&(z, _)| z.cmp(&hash_at_depth_max))
    {
      Ok(i) => {
        let e = &self.entries[i];
        Some((e.0, &e.1))
      }
      Err(i) => {
        if i > 0 {
          // if array len is 0, i will be 0 so we do not enter here.
          let e = &self.entries[i - 1];
          if Z::are_overlapping(hash_at_depth_max, e.0) {
            return Some((e.0, &e.1));
          }
        }
        if i < self.entries.len() {
          let e = &self.entries[i];
          if Z::are_overlapping(hash_at_depth_max, e.0) {
            return Some((e.0, &e.1));
          }
        }
        None
      }
    }
  }

  fn get_overlapped_cells(
    &'a self,
    zuniq: Self::ZUniqHType,
  ) -> Vec<(Self::ZUniqHType, &'a Self::ValueType)> {
    let mut range = match self.entries.binary_search_by(|&(z, _)| z.cmp(&zuniq)) {
      Ok(i) => i..i + 1,
      Err(i) => i..i,
    };
    while range.start - 1 > 0 && Z::are_overlapping(zuniq, self.entries[range.start - 1].0) {
      range.start -= 1;
    }
    while range.end < self.entries.len() && Z::are_overlapping(zuniq, self.entries[range.end].0) {
      range.end += 1;
    }
    range
      .into_iter()
      .map(|i| {
        let (z, v) = &self.entries[i];
        (*z, v)
      })
      .collect()
  }

  fn zuniqs(&'a self) -> Self::ZuniqIt {
    self.entries.iter().map(|&(zuniq, _)| zuniq)
  }

  fn values(&'a self) -> Self::ValuesIt {
    self.entries.iter().map(|(_, value)| value)
  }

  fn entries(&'a self) -> Self::EntriesIt {
    self.entries.iter().map(|(z, v)| (*z, v))
  }

  fn owned_entries(self) -> Self::OwnedEntriesIt {
    self.entries.into_iter()
  }

  /// # Params
  /// * `M`: merger function, i.e. function applied on the 4 values of 4 sibling cells
  /// (i.e. the 4 cells belonging to a same direct parent cell).
  /// The function decide whether value are merge (and how they are merged) or not returning
  ///either `Some` or `None`.
  fn from_skymap_ref<'s, S, M>(skymap: &'s S, merger: M) -> Self
  where
    S: SkyMap<'s, HashType = Z, ValueType = V>,
    M: Fn(&V, &V, &V, &V) -> Option<V>,
    V: 's,
  {
    let depth = skymap.depth();
    let mut entries: Vec<(Z, V)> = Vec::with_capacity(skymap.len());
    let mut expected_next_hash = Z::zero();
    for (h, v) in skymap.entries() {
      // To avoid the clone() here, we must accept an owned skymap
      // with an iterator (like Drain) iterating over the owned values.
      entries.push((Z::to_zuniq(depth, h), v.clone()));
      // Check that the value of the cell was the expected one and that
      // its values at the `depth` HEALPix layer (i.e last 2 LSB) is 3
      // (among the 4 possible values 0, 1, 2 and 3).
      if h == expected_next_hash && h & Z::LAST_LAYER_MASK == Z::LAST_LAYER_MASK {
        let n = entries.len();
        if let Some(combined_value) = merger(
          &entries[n - 4].1, // sibling 0
          &entries[n - 3].1, // sibling 1
          &entries[n - 2].1, // sibling 2
          &entries[n - 1].1, // sibling 3
        ) {
          let _ = entries.pop();
          let _ = entries.pop();
          let _ = entries.pop();
          // Unwrap ok here since we are sure that the array contained at least 4 entries
          // (we access them just above).
          let _ = entries.pop().unwrap();
          let new_zuniq = Z::to_zuniq(depth - 1, h >> 2);
          entries.push((new_zuniq, combined_value));
          Self::from_skymap_ref_recursive(&mut entries, &merger);
        }
      } else if h & Z::LAST_LAYER_MASK == Z::zero() {
        expected_next_hash = h;
      }
      expected_next_hash += Z::one();
    }
    Self { depth, entries }
  }

  fn from_skymap<'s, S, M>(skymap: S, merger: M) -> Self
  where
    S: SkyMap<'s, HashType = Self::ZUniqHType, ValueType = Self::ValueType>,
    M: Fn(
      Self::ValueType,
      Self::ValueType,
      Self::ValueType,
      Self::ValueType,
    ) -> Result<
      Self::ValueType,
      (
        Self::ValueType,
        Self::ValueType,
        Self::ValueType,
        Self::ValueType,
      ),
    >,
    Self::ValueType: 's,
  {
    let depth = skymap.depth();
    let mut entries: Vec<(Z, V)> = Vec::with_capacity(skymap.len());
    let mut expected_next_hash = Z::zero();
    for (h, v) in skymap.owned_entries() {
      entries.push((Z::to_zuniq(depth, h), v));
      // Check that the value of the cell was the expected one and that
      // its values at the `depth` HEALPix layer (i.e last 2 LSB) is 3
      // (among the 4 possible values 0, 1, 2 and 3).
      if h == expected_next_hash && h & Z::LAST_LAYER_MASK == Z::LAST_LAYER_MASK {
        let (z3, v3) = entries.pop().unwrap();
        let (z2, v2) = entries.pop().unwrap();
        let (z1, v1) = entries.pop().unwrap();
        let (z0, v0) = entries.pop().unwrap();
        match merger(v0, v1, v2, v3) {
          Ok(v_merged) => {
            let z_merged = Z::to_zuniq(depth - 1, h >> 2);
            entries.push((z_merged, v_merged));
            Self::from_skymap_recursive(&mut entries, &merger);
          }
          Err((v0, v1, v2, v3)) => {
            entries.push((z0, v0));
            entries.push((z1, v1));
            entries.push((z2, v2));
            entries.push((z3, v3));
          }
        }
      } else if h & Z::LAST_LAYER_MASK == Z::zero() {
        expected_next_hash = h;
      }
      expected_next_hash += Z::one();
    }
    Self { depth, entries }
  }

  ///! WARNING: No yet implemented!
  fn merge<'s, L, R, S, O, M>(lhs: L, rhs: R, split: S, op: O, merge: M) -> Self
  where
    L: Mom<'s, ZUniqHType = Z, ValueType = V>,
    R: Mom<'s, ZUniqHType = Z, ValueType = V>,
    S: Fn(
      Self::ValueType,
    ) -> (
      Self::ValueType,
      Self::ValueType,
      Self::ValueType,
      Self::ValueType,
    ),
    O: Fn(Option<Self::ValueType>, Option<Self::ValueType>) -> Option<Self::ValueType>,
    M: Fn(
      Self::ValueType,
      Self::ValueType,
      Self::ValueType,
      Self::ValueType,
    ) -> Result<
      Self::ValueType,
      (
        Self::ValueType,
        Self::ValueType,
        Self::ValueType,
        Self::ValueType,
      ),
    >,
  {
    #[derive(Clone)]
    struct DHZ<ZZ: ZUniqHashT> {
      d: u8,
      h: ZZ,
      z: ZZ,
    }
    impl<ZZ: ZUniqHashT> DHZ<ZZ> {
      fn new(d: u8, h: ZZ, z: ZZ) -> Self {
        Self { d, h, z }
      }
      fn next(&self) -> Self {
        Self::new(
          self.d,
          self.h + ZZ::one(),
          ZZ::to_zuniq(self.d, self.h + ZZ::one()),
        )
      }
    }
    struct DHZV<ZZ: ZUniqHashT, VV> {
      d: u8,
      h: ZZ,
      z: ZZ,
      v: VV,
    }
    impl<ZZ: ZUniqHashT, VV> DHZV<ZZ, VV> {
      fn new(d: u8, h: ZZ, z: ZZ, v: VV) -> Self {
        Self { d, h, z, v }
      }
    }
    let zv_to_dhzv = |(z, v)| {
      let (d, h) = Self::ZUniqHType::from_zuniq(z);
      DHZV { d, h, z, v }
    };
    let depth = lhs.depth_max().max(rhs.depth_max());
    let mut stack: Vec<(Z, V)> = Vec::with_capacity(lhs.len() + rhs.len());
    let mut it_left = lhs.owned_entries().map(zv_to_dhzv);
    let mut it_right = rhs.owned_entries().map(zv_to_dhzv);
    let mut lstack: Vec<DHZV<Z, V>> = Vec::with_capacity(128);
    let mut rstack: Vec<DHZV<Z, V>> = Vec::with_capacity(128);
    let mut left: Option<DHZV<Z, V>> = it_left.next();
    let mut right: Option<DHZV<Z, V>> = it_right.next();
    let mut expected_next_dhz = DHZ::new(depth, Z::zero(), Z::to_zuniq(depth, Z::zero()));
    let mut last_dhz_added = expected_next_dhz.clone();
    loop {
      match (left, right) {
        (Some(l), Some(r)) => match l.d.cmp(&r.d) {
          Ordering::Equal => match l.h.cmp(&r.h) {
            Ordering::Equal => {
              if let Some(v) = op(Some(l.v), Some(r.v)) {
                last_dhz_added = DHZ::new(l.d, l.h, l.z);
                stack.push((l.z, v));
              }
              left = if !lstack.is_empty() {
                lstack.pop()
              } else {
                it_left.next()
              };
              right = if !rstack.is_empty() {
                rstack.pop()
              } else {
                it_right.next()
              };
            }
            Ordering::Less => {
              if let Some(v) = op(Some(l.v), None) {
                last_dhz_added = DHZ::new(l.d, l.h, l.z);
                stack.push((l.z, v));
              }
              left = if !lstack.is_empty() {
                lstack.pop()
              } else {
                it_left.next()
              };
              right = Some(r);
            }
            Ordering::Greater => {
              if let Some(v) = op(None, Some(r.v)) {
                last_dhz_added = DHZ::new(r.d, r.h, r.z);
                stack.push((r.z, v));
              }
              right = if !rstack.is_empty() {
                rstack.pop()
              } else {
                it_right.next()
              };
              left = Some(l);
            }
          },
          Ordering::Less => {
            // degrade r_hash to l_depth
            // if equals, then split l (put in the stack)
            let r_hash_at_l_depth = r.h >> (((r.d - l.d) << 1) as usize);
            match l.h.cmp(&r_hash_at_l_depth) {
              Ordering::Equal => {
                let (v0, v1, v2, v3) = split(l.v);
                let new_d = l.d + 1;
                let new_h = l.h << 2;
                lstack.push(DHZV::new(
                  new_d,
                  new_h + Z::three(),
                  Z::to_zuniq(new_d, new_h + Z::three()),
                  v3,
                ));
                lstack.push(DHZV::new(
                  new_d,
                  new_h + Z::two(),
                  Z::to_zuniq(new_d, new_h + Z::two()),
                  v2,
                ));
                lstack.push(DHZV::new(
                  new_d,
                  new_h + Z::one(),
                  Z::to_zuniq(new_d, new_h + Z::one()),
                  v1,
                ));
                left = Some(DHZV::new(new_d, new_h, Z::to_zuniq(new_d, new_h), v0));
                right = Some(r);
              }
              Ordering::Less => {
                if let Some(v) = op(Some(l.v), None) {
                  last_dhz_added = DHZ::new(l.d, l.h, l.z);
                  stack.push((l.z, v));
                }
                left = if !lstack.is_empty() {
                  lstack.pop()
                } else {
                  it_left.next()
                };
                right = Some(r);
              }
              Ordering::Greater => {
                if let Some(v) = op(None, Some(r.v)) {
                  last_dhz_added = DHZ::new(r.d, r.h, r.z);
                  stack.push((r.z, v));
                }
                right = if !rstack.is_empty() {
                  rstack.pop()
                } else {
                  it_right.next()
                };
                left = Some(l);
              }
            }
          }
          Ordering::Greater => {
            let l_has_at_r_depth = l.h >> (((l.d - r.d) << 1) as usize);
            match l_has_at_r_depth.cmp(&r.h) {
              Ordering::Equal => {
                let (v0, v1, v2, v3) = split(r.v);
                let new_d = r.d + 1;
                let new_h = r.h << 2;
                rstack.push(DHZV::new(
                  new_d,
                  new_h + Z::three(),
                  Z::to_zuniq(new_d, new_h + Z::three()),
                  v3,
                ));
                rstack.push(DHZV::new(
                  new_d,
                  new_h + Z::two(),
                  Z::to_zuniq(new_d, new_h + Z::two()),
                  v2,
                ));
                rstack.push(DHZV::new(
                  new_d,
                  new_h + Z::one(),
                  Z::to_zuniq(new_d, new_h + Z::one()),
                  v1,
                ));
                left = Some(l);
                right = Some(DHZV::new(new_d, new_h, Z::to_zuniq(new_d, new_h), v0));
              }
              Ordering::Less => {
                if let Some(v) = op(Some(l.v), None) {
                  last_dhz_added = DHZ::new(l.d, l.h, l.z);
                  stack.push((l.z, v));
                }
                left = if !lstack.is_empty() {
                  lstack.pop()
                } else {
                  it_left.next()
                };
                right = Some(r);
              }
              Ordering::Greater => {
                if let Some(v) = op(None, Some(r.v)) {
                  last_dhz_added = DHZ::new(r.d, r.h, r.z);
                  stack.push((r.z, v));
                }
                right = if !rstack.is_empty() {
                  rstack.pop()
                } else {
                  it_right.next()
                };
                left = Some(l);
              }
            }
          }
        },
        (None, None) => break, // Important to let this test here!!!
        (mut left, None) => {
          while let Some(l) = left {
            if let Some(v) = op(Some(l.v), None) {
              last_dhz_added = DHZ::new(l.d, l.h, l.z);
              stack.push((l.z, v));
            }
            left = if !lstack.is_empty() {
              lstack.pop()
            } else {
              it_left.next()
            };
            if last_dhz_added.z == expected_next_dhz.z {
              if last_dhz_added.h & Z::LAST_LAYER_MASK == Z::LAST_LAYER_MASK {
                Self::from_skymap_recursive(&mut stack, &merge);
              } else {
                expected_next_dhz = last_dhz_added.next();
              }
            } else if last_dhz_added.h & Z::LAST_LAYER_MASK == Z::zero() {
              expected_next_dhz = last_dhz_added.next();
            }
          }
          break;
        }
        (None, mut right) => {
          while let Some(r) = right {
            if let Some(v) = op(None, Some(r.v)) {
              last_dhz_added = DHZ::new(r.d, r.h, r.z);
              stack.push((r.z, v));
            }
            right = if !rstack.is_empty() {
              rstack.pop()
            } else {
              it_right.next()
            };
            if last_dhz_added.z == expected_next_dhz.z {
              if last_dhz_added.h & Z::LAST_LAYER_MASK == Z::LAST_LAYER_MASK {
                Self::from_skymap_recursive(&mut stack, &merge);
              } else {
                expected_next_dhz = last_dhz_added.next();
              }
            } else if last_dhz_added.h & Z::LAST_LAYER_MASK == Z::zero() {
              expected_next_dhz = last_dhz_added.next();
            }
          }
          break;
        }
      }
      if last_dhz_added.z == expected_next_dhz.z {
        if last_dhz_added.h & Z::LAST_LAYER_MASK == Z::LAST_LAYER_MASK {
          Self::from_skymap_recursive(&mut stack, &merge);
        } else {
          expected_next_dhz = last_dhz_added.next();
        }
      } else if last_dhz_added.h & Z::LAST_LAYER_MASK == Z::zero() {
        expected_next_dhz = last_dhz_added.next();
      }
    }
    Self {
      depth,
      entries: stack,
    }
  }
}

impl<Z, V> MomVecImpl<Z, V>
where
  Z: ZUniqHashT,
  V: SkyMapValue,
{
  pub fn from<M, F>(mom: M, map: F) -> Self
  where
    M: for<'a> Mom<'a, ZUniqHType = Z>,
    F: Fn(<M as Mom>::ValueType) -> V,
  {
    let depth = mom.depth_max();
    let entries: Vec<(Z, V)> = mom.owned_entries().map(|(z, v)| (z, map(v))).collect();
    Self { depth, entries }
  }

  fn from_skymap_ref_recursive<'s, M>(stack: &mut Vec<(Z, V)>, merger: &M)
  where
    M: Fn(&V, &V, &V, &V) -> Option<V>,
    V: 's,
  {
    let n = stack.len();
    if n >= 4 {
      let e0 = &stack[n - 4];
      let (d0, h0) = Z::from_zuniq(e0.0);
      if d0 > 0 && h0 & Z::LAST_LAYER_MASK == Z::zero() {
        let e1 = &stack[n - 3];
        let e2 = &stack[n - 2];
        let e3 = &stack[n - 1];
        if e1.0 == Z::to_zuniq(d0, h0 + Z::one())
          && e2.0 == Z::to_zuniq(d0, h0 + Z::two())
          && e3.0 == Z::to_zuniq(d0, h0 + Z::three())
        {
          if let Some(combined_value) = merger(
            &e0.1, // sibling 0
            &e1.1, // sibling 1
            &e2.1, // sibling 2
            &e3.1, // sibling 3
          ) {
            let _ = stack.pop().unwrap();
            let _ = stack.pop().unwrap();
            let _ = stack.pop().unwrap();
            let _ = stack.pop().unwrap();
            let new_zuniq = Z::to_zuniq(d0 - 1, h0 >> 2);
            stack.push((new_zuniq, combined_value));
            Self::from_skymap_ref_recursive(stack, merger);
          }
        }
      }
    }
  }

  fn from_skymap_recursive<'s, M>(stack: &mut Vec<(Z, V)>, merger: &M)
  where
    M: Fn(V, V, V, V) -> Result<V, (V, V, V, V)>,
    V: 's,
  {
    let n = stack.len();
    if n >= 4 {
      let z0 = *&stack[n - 4].0;
      let (d0, h0) = Z::from_zuniq(z0);
      if d0 > 0 && h0 & Z::LAST_LAYER_MASK == Z::zero() {
        let z1 = *&stack[n - 3].0;
        let z2 = *&stack[n - 2].0;
        let z3 = *&stack[n - 1].0;
        if z1 == Z::to_zuniq(d0, h0 + Z::one())
          && z2 == Z::to_zuniq(d0, h0 + Z::two())
          && z3 == Z::to_zuniq(d0, h0 + Z::three())
        {
          let (_, v3) = stack.pop().unwrap();
          let (_, v2) = stack.pop().unwrap();
          let (_, v1) = stack.pop().unwrap();
          let (_, v0) = stack.pop().unwrap();
          match merger(v0, v1, v2, v3) {
            Ok(v_merged) => {
              let z_merged = Z::to_zuniq(d0 - 1, h0 >> 2);
              stack.push((z_merged, v_merged));
              Self::from_skymap_recursive(stack, merger);
            }
            Err((v0, v1, v2, v3)) => {
              stack.push((z0, v0));
              stack.push((z1, v1));
              stack.push((z2, v2));
              stack.push((z3, v3));
            }
          }
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use std::f64::consts::PI;

  use mapproj::pseudocyl::mol::Mol;

  use crate::{
    n_hash,
    nested::map::{
      img::{to_mom_png_file, ColorMapFunctionType, PosConversion},
      mom::{impls::zvec::MomVecImpl, Mom, ZUniqHashT},
      skymap::SkyMapEnum,
    },
  };

  #[test]
  #[cfg(not(target_arch = "wasm32"))]
  fn test_skymap_to_mom_basic() {
    let path = "test/resources/skymap/skymap.fits";
    // let path = "test/resources/skymap/gaiadr2.skymap.order10.fits";
    let skymap = SkyMapEnum::from_fits_file(path).unwrap();
    match skymap {
      SkyMapEnum::ImplicitU64I32(skymap) => {
        let merger = |n0: &i32, n1: &i32, n2: &i32, n3: &i32| -> Option<i32> {
          let sum = *n0 + *n1 + *n2 + *n3;
          if sum < 1_000_000 {
            Some(sum)
          } else {
            None
          }
        };
        let mut mom = MomVecImpl::from_skymap_ref(&skymap, merger);
        /*println!("Mom len: {}", mom.entries.len());
        for (z, v) in mom.entries {
          let (d, h) = u64::from_zuniq(z);
          println!("{},{},{}", d, h, v)
        }*/
        // assert_eq!(mom.len(), 1107);
        // Create a new MOM transforming number of sources into densities.
        let mom = MomVecImpl {
          depth: mom.depth,
          entries: mom
            .entries
            .drain(..)
            .map(|(z, v)| {
              (
                z,
                v as f64 / (4.0 * PI / (n_hash(u64::depth_from_zuniq(z))) as f64),
              )
            })
            .collect::<Vec<(u64, f64)>>(),
        };

        to_mom_png_file::<'_, _, Mol, _>(
          &mom,
          (1600, 800),
          None,
          None,
          None,
          Some(PosConversion::EqMap2GalImg),
          None,
          Some(ColorMapFunctionType::LinearLog), //Some(ColorMapFunctionType::LinearSqrt)
          "test/resources/skymap/mom.png",
          false,
        )
        .unwrap();
      }
      _ => assert!(false),
    }
  }

  #[test]
  #[cfg(not(target_arch = "wasm32"))]
  fn test_skymap_to_mom_chi2() {
    let path = "test/resources/skymap/skymap.fits";
    // let path = "test/resources/skymap/gaiadr2.skymap.order10.fits";
    let skymap = SkyMapEnum::from_fits_file(path).unwrap();
    match skymap {
      SkyMapEnum::ImplicitU64I32(skymap) => {
        // println!("Skymap size: {}", skymap.len());

        let merger = |n0: &i32, n1: &i32, n2: &i32, n3: &i32| -> Option<i32> {
          let mu0 = *n0 as f64;
          // let sig0 = mu0.sqrt();
          let mu1 = *n1 as f64;
          // let sig1 = mu1.sqrt();
          let mu2 = *n2 as f64;
          // let sig2 = mu2.sqrt();
          let mu3 = *n3 as f64;
          // let sig3 = mu3.sqrt();

          let sum = mu0 + mu1 + mu2 + mu3;
          let weighted_var_inv = 1.0 / mu0 + 1.0 / mu1 + 1.0 / mu2 + 1.0 / mu3;
          let weighted_mean = 4.0 / weighted_var_inv;
          let chi2_of_3dof = sum - 4.0 * weighted_mean;
          // chi2 3 dof:
          // 90.0% =>  6.251
          // 95.0% =>  7.815
          // 97.5% =>  9.348
          // 99.0% => 11.345
          // 99.9% => 16.266
          if chi2_of_3dof < 16.266 {
            Some(*n0 + *n1 + *n2 + *n3)
          } else {
            None
          }
        };
        let mut mom = MomVecImpl::from_skymap_ref(&skymap, merger);
        /*println!("Mom len: {}", mom.entries.len());
        for (z, v) in mom.entries {
          let (d, h) = u64::from_zuniq(z);
          println!("{},{},{}", d, h, v)
        }*/
        // assert_eq!(mom.len(), 1107);

        // println!("MOM size: {}", mom.len());

        // Create a new MOM transforming number of sources into densities.
        let mom = MomVecImpl {
          depth: mom.depth,
          entries: mom
            .entries
            .drain(..)
            .map(|(z, v)| {
              (
                z,
                v as f64 / (4.0 * PI / (n_hash(u64::depth_from_zuniq(z))) as f64),
              )
            })
            .collect::<Vec<(u64, f64)>>(),
        };

        to_mom_png_file::<'_, _, Mol, _>(
          &mom,
          (1600, 800),
          None,
          None,
          None,
          Some(PosConversion::EqMap2GalImg),
          None,
          Some(ColorMapFunctionType::LinearLog), //Some(ColorMapFunctionType::LinearSqrt)
          "test/resources/skymap/mom.png",
          false,
        )
        .unwrap();
      }
      _ => assert!(false),
    }
  }
}
