use std::io::BufRead;
use std::{
  error::Error,
  f64::consts::PI,
  fs::File,
  io::{BufReader, BufWriter, Error as IoError, Read, Seek, Write},
  iter::{Enumerate, Map},
  marker::PhantomData,
  ops::{Add, AddAssign, Deref, RangeInclusive},
  path::Path,
  slice::Iter,
  vec::IntoIter,
};

use colorous::Gradient;
use itertools::Itertools;
use log::error;
use mapproj::CanonicalProjection;
use num_traits::ToBytes;
use rayon::prelude::{ParallelSlice, ParallelSliceMut};
use rayon::{
  prelude::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefMutIterator, ParallelIterator,
  },
  ThreadPool,
};

use crate::{
  n_hash,
  nested::{
    get,
    map::{
      mom::{impls::zvec::MomVecImpl, Mom, ZUniqHashT},
      HHash,
    },
  },
};

#[cfg(not(target_arch = "wasm32"))]
use super::img::show_with_default_app;
use super::{
  fits::{error::FitsError, read::from_fits_skymap, write::write_implicit_skymap_fits},
  img::{to_skymap_png, ColorMapFunctionType, PosConversion},
};

/// Trait marking the type of the values writable in a FITS skymap.
pub trait SkyMapValue: ToBytes + Add + AddAssign + Clone {
  /// FITS size, in bytes, of a value.
  fn fits_naxis1() -> u8;
  /// FITS type of the value
  fn fits_tform() -> &'static str;
}

impl SkyMapValue for u8 {
  fn fits_naxis1() -> u8 {
    size_of::<Self>() as u8
  }
  fn fits_tform() -> &'static str {
    "B"
  }
}
impl SkyMapValue for i16 {
  fn fits_naxis1() -> u8 {
    size_of::<Self>() as u8
  }
  fn fits_tform() -> &'static str {
    "I"
  }
}
impl SkyMapValue for i32 {
  fn fits_naxis1() -> u8 {
    4
  }
  fn fits_tform() -> &'static str {
    "J"
  }
}
impl SkyMapValue for i64 {
  fn fits_naxis1() -> u8 {
    size_of::<Self>() as u8
  }
  fn fits_tform() -> &'static str {
    "K"
  }
}
impl SkyMapValue for u32 {
  fn fits_naxis1() -> u8 {
    size_of::<Self>() as u8
  }
  fn fits_tform() -> &'static str {
    "J"
  }
}
impl SkyMapValue for u64 {
  fn fits_naxis1() -> u8 {
    size_of::<Self>() as u8
  }
  fn fits_tform() -> &'static str {
    "K"
  }
}
impl SkyMapValue for f32 {
  fn fits_naxis1() -> u8 {
    size_of::<Self>() as u8
  }
  fn fits_tform() -> &'static str {
    "E"
  }
}
impl SkyMapValue for f64 {
  fn fits_naxis1() -> u8 {
    size_of::<Self>() as u8
  }
  fn fits_tform() -> &'static str {
    "D"
  }
}

pub trait SkyMap<'a> {
  /// Type of the HEALPix hash value (mainly `u32` or `u64`).
  type HashType: HHash;
  /// Type of the value associated to each HEALPix cell.
  type ValueType: 'a + SkyMapValue;
  /// Type of the iterator iterating on the skymap values.
  type ValuesIt: Iterator<Item = &'a Self::ValueType>;
  /// Type of the iterator iterating on the skymap borrowed entries.
  /// WARNING: we are so far stucked with iterator on ranges,
  /// e.g `(0..n_cell).iter().zip(...)`, since it relies on the `Step` trait
  /// which requires `nightly builds`.
  /// In the case of `implicit` skymaps, a solution is to use `enumerate`.
  type EntriesIt: Iterator<Item = (Self::HashType, &'a Self::ValueType)>;
  /// Type of iterator iterating on owned entries.
  type OwnedEntriesIt: Iterator<Item = (Self::HashType, Self::ValueType)>;

  /// Depth (<=> HEALPix order) of the skymap.
  fn depth(&self) -> u8;

  /// Tells whether the map is implicit or not.
  /// If implicit, method `values` and `entries` will return as many items as the number
  /// of HEALPix cell at the map HEALPix depth.
  fn is_implicit(&self) -> bool;

  /// Returns the number of elements in the skymap.
  /// For implicit skymaps, the number of elements equals the number of HEALPix cells at the
  /// skymap depth/order.
  fn len(&self) -> usize;

  /// Returns the value associated with the HEALPix cell of given hash number.
  fn get(&self, hash: Self::HashType) -> &Self::ValueType;

  /// Returns all values associated with HEALPix cells, ordered by increasing cell hash number.
  fn values(&'a self) -> Self::ValuesIt;

  /// Returns all entries, i.e. HEALPix cell hash / value tuples, ordered by increasing cell hash number.
  fn entries(&'a self) -> Self::EntriesIt;

  /// In case we want to build mom from complex type that are costly to clone.
  fn owned_entries(self) -> Self::OwnedEntriesIt;
}

#[derive(Debug)]
pub struct ImplicitSkyMapArray<H: HHash, V: SkyMapValue> {
  depth: u8,
  values: Box<[V]>,
  _htype: PhantomData<H>,
}
impl<'a, H: HHash, V: SkyMapValue + 'a> ImplicitSkyMapArray<H, V> {
  /// WARNING: we assume that the coherency between the depth and the number of elements in the
  ///array has already been tested.
  pub fn new(depth: u8, values: Box<[V]>) -> Self {
    assert_eq!(
      n_hash(depth) as usize,
      values.deref().len(),
      "Wrong implicit skymap size. Epecgted: {}. Actual: {}.",
      n_hash(depth),
      values.len()
    );
    Self {
      depth,
      values,
      _htype: PhantomData,
    }
  }
}
impl<'a, H: HHash, V: SkyMapValue + Send + Sync + AddAssign + 'a> ImplicitSkyMapArray<H, V> {
  pub fn par_add(mut self, rhs: Self) -> Self {
    self
      .values
      .as_parallel_slice_mut()
      .par_iter_mut()
      .zip_eq(rhs.values.into_vec().into_par_iter())
      .for_each(|(l, r)| l.add_assign(r));
    self
  }
}
impl<'a, H: HHash, V: SkyMapValue + 'a> SkyMap<'a> for ImplicitSkyMapArray<H, V> {
  type HashType = H;
  type ValueType = V;
  type ValuesIt = Iter<'a, Self::ValueType>;
  type EntriesIt = Map<Enumerate<Self::ValuesIt>, fn((usize, &V)) -> (H, &V)>;
  type OwnedEntriesIt = Map<Enumerate<IntoIter<Self::ValueType>>, fn((usize, V)) -> (H, V)>;

  fn depth(&self) -> u8 {
    self.depth
  }

  fn is_implicit(&self) -> bool {
    true
  }

  fn len(&self) -> usize {
    self.values.len()
  }

  fn get(&self, hash: Self::HashType) -> &Self::ValueType {
    &self.values.deref()[hash.as_()]
  }

  fn values(&'a self) -> Self::ValuesIt {
    self.values.deref().iter()
  }

  fn entries(&'a self) -> Self::EntriesIt {
    self
      .values
      .deref()
      .iter()
      .enumerate()
      .map(move |(h, v)| (H::from_usize(h), v))
  }

  fn owned_entries(self) -> Self::OwnedEntriesIt {
    self
      .values
      .to_vec()
      .into_iter()
      .enumerate()
      .map(move |(h, v)| (H::from_usize(h), v))
  }
}

#[derive(Debug)]
pub struct ImplicitSkyMapArrayRef<'a, H: HHash, V: SkyMapValue> {
  depth: u8,
  values: &'a [V],
  _htype: PhantomData<H>,
}
impl<'a, H: HHash, V: SkyMapValue + 'a> ImplicitSkyMapArrayRef<'a, H, V> {
  /// WARNING: we assume that the coherency between the depth and the number of elements in the
  ///array has already been tested.
  pub fn new(depth: u8, values: &'a [V]) -> Self {
    assert_eq!(
      n_hash(depth) as usize,
      values.len(),
      "Wrong implicit skymap size. Epecgted: {}. Actual: {}.",
      n_hash(depth),
      values.len()
    );
    Self {
      depth,
      values,
      _htype: PhantomData,
    }
  }

  /*pub fn par_add(mut self, rhs: Self, pool: &ThreadPool) -> Self {
    pool.install(|| {
      self
        .values
        .par_iter_mut()
        .zip_eq(rhs.values.into_par_iter())
        .for_each(|(l, r)| *l += r)
    });
    self
  }*/
}
impl<'a, H: HHash, V: SkyMapValue + Clone + 'a> SkyMap<'a> for ImplicitSkyMapArrayRef<'a, H, V> {
  type HashType = H;
  type ValueType = V;
  type ValuesIt = Iter<'a, Self::ValueType>;
  type EntriesIt = Map<Enumerate<Self::ValuesIt>, fn((usize, &V)) -> (H, &V)>;
  type OwnedEntriesIt = Map<Enumerate<Self::ValuesIt>, fn((usize, &V)) -> (H, V)>;

  fn depth(&self) -> u8 {
    self.depth
  }

  fn is_implicit(&self) -> bool {
    true
  }

  fn len(&self) -> usize {
    self.values.len()
  }

  fn get(&self, hash: Self::HashType) -> &Self::ValueType {
    &self.values[hash.as_()]
  }

  fn values(&'a self) -> Self::ValuesIt {
    self.values.iter()
  }

  fn entries(&'a self) -> Self::EntriesIt {
    self
      .values
      .iter()
      .enumerate()
      .map(move |(h, v)| (H::from_usize(h), v))
  }

  // Make a owned_values method!!

  fn owned_entries(self) -> Self::OwnedEntriesIt {
    self
      .values
      .iter()
      .enumerate()
      .map(move |(h, v)| (H::from_usize(h), v.clone()))
  }
}

#[derive(Debug)]
pub enum SkyMapEnum {
  ImplicitU64U8(ImplicitSkyMapArray<u64, u8>),
  ImplicitU64I16(ImplicitSkyMapArray<u64, i16>),
  ImplicitU64I32(ImplicitSkyMapArray<u64, i32>),
  ImplicitU64I64(ImplicitSkyMapArray<u64, i64>),
  ImplicitU64F32(ImplicitSkyMapArray<u64, f32>),
  ImplicitU64F64(ImplicitSkyMapArray<u64, f64>),
}

impl SkyMapEnum {
  #[cfg(not(target_arch = "wasm32"))]
  pub fn from_fits_file<P: AsRef<Path>>(path: P) -> Result<Self, FitsError> {
    File::open(path)
      .map_err(FitsError::Io)
      .map(BufReader::new)
      .and_then(SkyMapEnum::from_fits)
  }

  pub fn from_fits<R: Read + Seek>(reader: BufReader<R>) -> Result<Self, FitsError> {
    from_fits_skymap(reader)
  }

  pub fn to_fits<W: Write>(&self, writer: W) -> Result<(), FitsError> {
    match &self {
      Self::ImplicitU64U8(s) => write_implicit_skymap_fits(writer, s.values.deref()),
      Self::ImplicitU64I16(s) => write_implicit_skymap_fits(writer, s.values.deref()),
      Self::ImplicitU64I32(s) => write_implicit_skymap_fits(writer, s.values.deref()),
      Self::ImplicitU64I64(s) => write_implicit_skymap_fits(writer, s.values.deref()),
      Self::ImplicitU64F32(s) => write_implicit_skymap_fits(writer, s.values.deref()),
      Self::ImplicitU64F64(s) => write_implicit_skymap_fits(writer, s.values.deref()),
    }
  }

  pub fn to_fits_file<P: AsRef<Path>>(&self, path: P) -> Result<(), FitsError> {
    File::create(path)
      .map_err(|e| FitsError::Io(e))
      .and_then(|file| self.to_fits(BufWriter::new(file)))
  }

  #[cfg(not(target_arch = "wasm32"))]
  pub fn to_skymap_png_file<P: CanonicalProjection, W: AsRef<Path>>(
    &self,
    img_size: (u16, u16),
    proj: Option<P>,
    proj_center: Option<(f64, f64)>,
    proj_bounds: Option<(RangeInclusive<f64>, RangeInclusive<f64>)>,
    pos_convert: Option<PosConversion>,
    color_map: Option<Gradient>,
    color_map_func_type: Option<ColorMapFunctionType>,
    path: W,
    view: bool,
  ) -> Result<(), Box<dyn Error>> {
    File::create(path.as_ref())
      .map_err(|e| e.into())
      .map(BufWriter::new)
      .and_then(|mut writer| {
        self.to_skymap_png(
          img_size,
          proj,
          proj_center,
          proj_bounds,
          pos_convert,
          color_map,
          color_map_func_type,
          &mut writer,
        )
      })
      .and_then(|()| {
        if view {
          show_with_default_app(path.as_ref().to_string_lossy().as_ref()).map_err(|e| e.into())
        } else {
          Ok(())
        }
      })
  }

  pub fn to_skymap_png<P: CanonicalProjection, W: Write>(
    &self,
    img_size: (u16, u16),
    proj: Option<P>,
    proj_center: Option<(f64, f64)>,
    proj_bounds: Option<(RangeInclusive<f64>, RangeInclusive<f64>)>,
    pos_convert: Option<PosConversion>,
    color_map: Option<Gradient>,
    color_map_func_type: Option<ColorMapFunctionType>,
    writer: W,
  ) -> Result<(), Box<dyn Error>> {
    match &self {
      Self::ImplicitU64U8(s) => to_skymap_png(
        s,
        img_size,
        proj,
        proj_center,
        proj_bounds,
        pos_convert,
        color_map,
        color_map_func_type,
        writer,
      ),
      Self::ImplicitU64I16(s) => to_skymap_png(
        s,
        img_size,
        proj,
        proj_center,
        proj_bounds,
        pos_convert,
        color_map,
        color_map_func_type,
        writer,
      ),
      Self::ImplicitU64I32(s) => to_skymap_png(
        s,
        img_size,
        proj,
        proj_center,
        proj_bounds,
        pos_convert,
        color_map,
        color_map_func_type,
        writer,
      ),
      Self::ImplicitU64I64(s) => to_skymap_png(
        s,
        img_size,
        proj,
        proj_center,
        proj_bounds,
        pos_convert,
        color_map,
        color_map_func_type,
        writer,
      ),
      Self::ImplicitU64F32(s) => to_skymap_png(
        s,
        img_size,
        proj,
        proj_center,
        proj_bounds,
        pos_convert,
        color_map,
        color_map_func_type,
        writer,
      ),
      Self::ImplicitU64F64(s) => to_skymap_png(
        s,
        img_size,
        proj,
        proj_center,
        proj_bounds,
        pos_convert,
        color_map,
        color_map_func_type,
        writer,
      ),
    }
  }
}

/// SkyMap implementation use to store counts.
#[derive(Debug)]
pub struct CountMap(ImplicitSkyMapArray<u64, u32>);
impl CountMap {
  pub fn as_implicit_skymap_array(&self) -> &ImplicitSkyMapArray<u64, u32> {
    &self.0
  }
  pub fn into_implicit_skymap_array(self) -> ImplicitSkyMapArray<u64, u32> {
    self.0
  }
  /// Build a count skymap from an iterator over HEALPix cells at the given depth.
  /// # Panics
  /// * if `depth > 12`.
  pub fn from_hash_values<I>(depth: u8, hash_values_it: I) -> Self
  where
    I: Iterator<Item = u32>,
  {
    assert!(
      depth < 13,
      "Wrong count map input depth. Expected: < 13. Actual: {}",
      depth
    );
    let mut counts = vec![0_u32; n_hash(depth) as usize].into_boxed_slice();
    for h in hash_values_it {
      counts[h as usize] += 1;
    }
    Self(ImplicitSkyMapArray::new(depth, counts))
  }

  /// Build a count skymap from an iterator over position ( (ra, dec), in radian).
  /// # Panics
  /// * if `depth > 12`.
  pub fn from_positions<I>(depth: u8, pos_it_rad: I) -> Self
  where
    I: Iterator<Item = (f64, f64)>,
  {
    assert!(
      depth < 13,
      "Wrong count map input depth. Expected: < 13. Actual: {}",
      depth
    );
    let layer = get(depth);
    let mut counts = vec![0_u32; layer.n_hash as usize].into_boxed_slice();
    for (l, b) in pos_it_rad {
      counts[layer.hash(l, b) as usize] += 1;
    }
    Self(ImplicitSkyMapArray::new(depth, counts))
  }

  pub fn par_add(mut self, rhs: Self) -> Self {
    /*self
    .0
    .values
    .par_iter_mut()
    .zip_eq(rhs.0.values.into_par_iter())
    .for_each(|(l, r)| *l += r)*/
    self.0 = self.0.par_add(rhs.0);
    self
  }

  pub fn to_fits<W: Write>(&self, writer: W) -> Result<(), FitsError> {
    write_implicit_skymap_fits(writer, self.as_implicit_skymap_array().values.deref())
  }

  pub fn to_fits_file<P: AsRef<Path>>(&self, path: P) -> Result<(), FitsError> {
    File::create(path)
      .map_err(|e| FitsError::Io(e))
      .and_then(|file| self.to_fits(BufWriter::new(file)))
  }

  pub fn to_chi2_mom(&self) -> MomVecImpl<u64, f64> {
    let chi2_merger = |_depth: u8, _hash: u64, [n0, n1, n2, n3]: [&u32; 4]| -> Option<u32> {
      // With Poisson distribution:
      // * mu_i = source density in cell i
      // * sigma_i = sqrt(mu_i)
      // weight_i = 1 / sigma_i^2
      // mu_e = weighted_mean = ( sum_{1=1}^4 weight_i * mu_i ) / ( sum_{1=1}^4 weight_i )
      //                      = 4 / ( sum_{1=1}^4 1/mu_i )
      // V_e^{-1} = sum_{1=1}^4 1/mu_i
      // Applying Pineau et al. 2017:
      // => sum_{1=1}^4 (mu_i - mu_e)^2 / mu_i = ... = (sum_{1=1}^4 mu_i) - 4 * mu_e
      // Normal law product of 4 1D normal laws and apply Pineau 2017 to find the above equation:
      // 1/sqrt(2 * pi) * exp[ -1/2 * sum_{i=1}^4 ( (x - mu_i)/sqrt(sigma_i) )^2] / sqrt(prod_{i=1}^4 sigma_i)

      let mu0 = *n0 as f64;
      let mu1 = *n1 as f64;
      let mu2 = *n2 as f64;
      let mu3 = *n3 as f64;

      let sum = mu0 + mu1 + mu2 + mu3;
      let weighted_var_inv =
        1.0 / mu0.max(1.0) + 1.0 / mu1.max(1.0) + 1.0 / mu2.max(1.0) + 1.0 / mu3.max(1.0);
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
    let mom = MomVecImpl::from_skymap_ref(&self.0, chi2_merger);
    // Create a new MOM transforming number of sources into densities.
    let mom = MomVecImpl::from(mom, |z, v| {
      v as f64 / (4.0 * PI / (n_hash(u64::depth_from_zuniq(z))) as f64)
    });
    mom
  }

  // to_png
  // to_fits
}
impl Add for CountMap {
  type Output = Self;

  fn add(mut self, rhs: Self) -> Self::Output {
    self
      .0
      .values
      .iter_mut()
      .zip_eq(rhs.0.values.iter())
      .for_each(|(l, r)| *l += r);
    self
  }
}
impl<'a> SkyMap<'a> for CountMap {
  type HashType = u64;
  type ValueType = u32;
  type ValuesIt = Iter<'a, Self::ValueType>;
  type EntriesIt = Map<Enumerate<Self::ValuesIt>, fn((usize, &u32)) -> (u64, &u32)>;
  type OwnedEntriesIt = Map<Enumerate<IntoIter<Self::ValueType>>, fn((usize, u32)) -> (u64, u32)>;

  fn depth(&self) -> u8 {
    self.as_implicit_skymap_array().depth
  }

  fn is_implicit(&self) -> bool {
    true
  }

  fn len(&self) -> usize {
    self.as_implicit_skymap_array().values.len()
  }

  fn get(&self, hash: Self::HashType) -> &Self::ValueType {
    &self.as_implicit_skymap_array().values.deref()[hash as usize]
  }

  fn values(&'a self) -> Self::ValuesIt {
    self.as_implicit_skymap_array().values.deref().iter()
  }

  fn entries(&'a self) -> Self::EntriesIt {
    self
      .as_implicit_skymap_array()
      .values
      .deref()
      .iter()
      .enumerate()
      .map(move |(h, v)| (u64::from_usize(h), v))
  }

  fn owned_entries(self) -> Self::OwnedEntriesIt {
    self
      .into_implicit_skymap_array()
      .values
      .to_vec()
      .into_iter()
      .enumerate()
      .map(move |(h, v)| (u64::from_usize(h), v))
  }
}

/// SkyMap implementation use to store counts, rely on u32 HEALPix index instead of u64.
#[derive(Debug)]
pub struct CountMapU32(ImplicitSkyMapArray<u32, u32>);
impl CountMapU32 {
  pub fn as_implicit_skymap_array(&self) -> &ImplicitSkyMapArray<u32, u32> {
    &self.0
  }
  pub fn into_implicit_skymap_array(self) -> ImplicitSkyMapArray<u32, u32> {
    self.0
  }
  /// Build a count skymap from an iterator over HEALPix cells at the given depth.
  /// # Panics
  /// * if `depth > 12`.
  pub fn from_hash_values<I>(depth: u8, hash_values_it: I) -> Self
  where
    I: Iterator<Item = u32>,
  {
    assert!(
      depth < 13,
      "Wrong count map input depth. Expected: < 13. Actual: {}",
      depth
    );
    let mut counts = vec![0_u32; n_hash(depth) as usize].into_boxed_slice();
    for h in hash_values_it {
      counts[h as usize] += 1;
    }
    Self(ImplicitSkyMapArray::new(depth, counts))
  }

  /// Build a count skymap from an iterator over position ( (ra, dec), in radian).
  /// # Panics
  /// * if `depth > 12`.
  pub fn from_positions<I>(depth: u8, pos_it_rad: I) -> Self
  where
    I: Iterator<Item = (f64, f64)>,
  {
    assert!(
      depth < 13,
      "Wrong count map input depth. Expected: < 13. Actual: {}",
      depth
    );
    let layer = get(depth);
    let mut counts = vec![0_u32; layer.n_hash as usize].into_boxed_slice();
    for (l, b) in pos_it_rad {
      counts[layer.hash(l, b) as usize] += 1;
    }
    Self(ImplicitSkyMapArray::new(depth, counts))
  }

  /*pub fn from_csv_par() -> Result<Self, Box<dyn Error>> {

  }*/

  pub fn from_csv_file_par<P: AsRef<Path>>(
    path: P,
    ilon: usize,
    ilat: usize,
    separator: Option<char>,
    has_header: bool,
    depth: u8,
    chunk_size: usize,
    thread_pool: &ThreadPool,
  ) -> Result<Self, IoError> {
    let mut it = BufReader::new(File::open(&path)?).lines().peekable();
    // Handle starting comments
    while let Some(Ok(_)) = it.next_if(|res| {
      res
        .as_ref()
        .map(|line| line.starts_with('#'))
        .unwrap_or(false)
    }) {}
    // Handle header line
    if has_header {
      it.next().transpose()?;
    }
    // Ok, go!
    Self::from_csv_it_par(it, ilon, ilat, separator, depth, chunk_size, thread_pool)
  }

  /// Compute the count map from an iterator iterating on raw CSV rows.
  /// # Params
  /// * `it` the iterator an rows
  /// * `ilon` index of the column containing the longitude
  /// * `ilat` index of the column containing the latitude
  /// * `separator` file separator (',' if None)
  /// * `depth` HEALPix depth (or order) of the map
  /// * `chunk_size` number of rows to be processed in parallel (the memory will hold twice this number
  ///   since a chunk is read while another chunk is processed)
  /// * `thread_pool` the thread pool in which the process will be executed
  /// # Note
  /// The parallel processing chosen resort on a single sequential read.
  /// Although it is not the fastest option with SSDs, it should ensure reasonably good performances
  /// with both SSDs and HDDs.
  /// # Warning
  /// Ensure that you have already removed the comment and the possible header line,
  /// E.g. using:
  /// ```rust,ignore
  ///     let mut it = it.peekable();
  ///     // Handle starting comments
  ///     while let Some(Ok(line)) = it.next_if(|res| {
  ///       res
  ///         .as_ref()
  ///         .map(|line| line.starts_with('#'))
  ///         .unwrap_or(false)
  ///     }) { }
  ///     // Handle header line
  ///     if has_header {
  ///       it.next().transpose()?;
  ///     }
  /// ```
  pub fn from_csv_it_par<I>(
    mut it: I,
    ilon: usize,
    ilat: usize,
    separator: Option<char>,
    depth: u8,
    chunk_size: usize,
    thread_pool: &ThreadPool,
  ) -> Result<Self, IoError>
  where
    I: Iterator<Item = Result<String, IoError>> + Send,
  {
    let separator = separator.unwrap_or(',');
    let layer = get(depth);
    let n_hash = layer.n_hash as usize;
    let n_thread = thread_pool.current_num_threads();
    let hpx = move |s: &String| {
      let cols = s.split(separator).collect::<Vec<&str>>();
      match (cols[ilon].parse::<f64>(), cols[ilat].parse::<f64>()) {
        (Ok(lon), Ok(lat)) => Some(layer.hash(lon.to_radians(), lat.to_radians()) as u32),
        _ => {
          error!("Error parsing coordinates at line: {}. Hash set to 0.", s);
          None
        }
      }
    };
    let count_map_fn = |chunk: Vec<String>| {
      chunk
        .par_chunks((chunk.len() / (n_thread << 2)).max(10_000))
        .map(|elems| CountMapU32::from_hash_values(depth, elems.iter().filter_map(hpx)))
        .reduce_with(|mapl, mapr| mapl.par_add(mapr))
    };
    fn load_n<I: Iterator<Item = Result<String, IoError>>>(
      chunk_size: usize,
      it: &mut I,
    ) -> Result<Vec<String>, IoError> {
      it.take(chunk_size).collect()
    }

    let mut chunk = load_n(chunk_size, it.by_ref())?;
    let mut count_map = Self(ImplicitSkyMapArray::new(
      depth,
      vec![0_u32; n_hash].into_boxed_slice(),
    ));
    while !chunk.is_empty() {
      let (next_chunk, new_count_map) = thread_pool.join(
        || load_n(chunk_size, it.by_ref()),
        || {
          if let Some(local_count_map) = count_map_fn(chunk) {
            count_map.par_add(local_count_map)
          } else {
            count_map
          }
        },
      );
      chunk = next_chunk?;
      count_map = new_count_map;
    }
    Ok(count_map)
  }

  pub fn par_add(mut self, rhs: Self) -> Self {
    /* self
    .0
    .values
    .par_iter_mut()
    .zip_eq(rhs.0.values.into_par_iter())
    .for_each(|(l, r)| *l += r)*/
    self.0 = self.0.par_add(rhs.0);
    self
  }

  pub fn to_fits<W: Write>(&self, writer: W) -> Result<(), FitsError> {
    write_implicit_skymap_fits(writer, self.as_implicit_skymap_array().values.deref())
  }

  pub fn to_fits_file<P: AsRef<Path>>(&self, path: P) -> Result<(), FitsError> {
    File::create(&path)
      .map_err(|err| FitsError::IoWithPath {
        path: path.as_ref().to_string_lossy().into(),
        err,
      })
      .and_then(|file| self.to_fits(BufWriter::new(file)))
  }

  pub fn to_chi2_mom(&self) -> MomVecImpl<u32, f64> {
    let chi2_merger = |_depth: u8, _hash: u32, [n0, n1, n2, n3]: [&u32; 4]| -> Option<u32> {
      // With Poisson distribution:
      // * mu_i = source density in cell i
      // * sigma_i = sqrt(mu_i)
      // weight_i = 1 / sigma_i^2
      // mu_e = weighted_mean = ( sum_{1=1}^4 weight_i * mu_i ) / ( sum_{1=1}^4 weight_i )
      //                      = 4 / ( sum_{1=1}^4 1/mu_i )
      // V_e^{-1} = sum_{1=1}^4 1/mu_i
      // Applying Pineau et al. 2017:
      // => sum_{1=1}^4 (mu_i - mu_e)^2 / mu_i = ... = (sum_{1=1}^4 mu_i) - 4 * mu_e
      // Normal law product of 4 1D normal laws and apply Pineau 2017 to find the above equation:
      // 1/sqrt(2 * pi) * exp[ -1/2 * sum_{i=1}^4 ( (x - mu_i)/sqrt(sigma_i) )^2] / sqrt(prod_{i=1}^4 sigma_i)

      let mu0 = *n0 as f64;
      let mu1 = *n1 as f64;
      let mu2 = *n2 as f64;
      let mu3 = *n3 as f64;

      let sum = mu0 + mu1 + mu2 + mu3;
      let weighted_var_inv =
        1.0 / mu0.max(1.0) + 1.0 / mu1.max(1.0) + 1.0 / mu2.max(1.0) + 1.0 / mu3.max(1.0);
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
    let mom = MomVecImpl::from_skymap_ref(&self.0, chi2_merger);
    // Create a new MOM transforming number of sources into densities.
    let mom = MomVecImpl::from(mom, |z, v| {
      v as f64 / (4.0 * PI / (n_hash(u32::depth_from_zuniq(z))) as f64)
    });
    mom
  }

  // to_png
  // to_fits
}
impl Add for CountMapU32 {
  type Output = Self;

  fn add(mut self, rhs: Self) -> Self::Output {
    self
      .0
      .values
      .iter_mut()
      .zip_eq(rhs.0.values.iter())
      .for_each(|(l, r)| *l += r);
    self
  }
}
impl<'a> SkyMap<'a> for CountMapU32 {
  type HashType = u32;
  type ValueType = u32;
  type ValuesIt = Iter<'a, Self::ValueType>;
  type EntriesIt = Map<Enumerate<Self::ValuesIt>, fn((usize, &u32)) -> (u32, &u32)>;
  type OwnedEntriesIt = Map<Enumerate<IntoIter<Self::ValueType>>, fn((usize, u32)) -> (u32, u32)>;

  fn depth(&self) -> u8 {
    self.as_implicit_skymap_array().depth
  }

  fn is_implicit(&self) -> bool {
    true
  }

  fn len(&self) -> usize {
    self.as_implicit_skymap_array().values.len()
  }

  fn get(&self, hash: Self::HashType) -> &Self::ValueType {
    &self.as_implicit_skymap_array().values.deref()[hash as usize]
  }

  fn values(&'a self) -> Self::ValuesIt {
    self.as_implicit_skymap_array().values.deref().iter()
  }

  fn entries(&'a self) -> Self::EntriesIt {
    self
      .as_implicit_skymap_array()
      .values
      .deref()
      .iter()
      .enumerate()
      .map(move |(h, v)| (u32::from_usize(h), v))
  }

  fn owned_entries(self) -> Self::OwnedEntriesIt {
    self
      .into_implicit_skymap_array()
      .values
      .to_vec()
      .into_iter()
      .enumerate()
      .map(move |(h, v)| (u32::from_usize(h), v))
  }
}

/// SkyMap implementation use to store densities.
#[derive(Debug)]
pub struct DensityMap(ImplicitSkyMapArray<u32, f64>);
impl DensityMap {
  pub fn as_implicit_skymap_array(&self) -> &ImplicitSkyMapArray<u32, f64> {
    &self.0
  }
  pub fn into_implicit_skymap_array(self) -> ImplicitSkyMapArray<u32, f64> {
    self.0
  }
  /// Build a count skymap from an iterator over position ( (ra, dec), in radian).
  /// # Panics
  /// * if `depth > 12`.
  pub fn from_positions<I>(depth: u8, pos_it_rad: I) -> Self
  where
    I: Iterator<Item = (f64, f64)>,
  {
    assert!(
      depth < 13,
      "Wrong count map input depth. Expected: < 13. Actual: {}",
      depth
    );
    let layer = get(depth);
    let mut densities = vec![0_f64; layer.n_hash as usize].into_boxed_slice();
    let one_over_cell_area = layer.n_hash as f64 / (4.0 * PI);
    for (l, b) in pos_it_rad {
      densities[layer.hash(l, b) as usize] += one_over_cell_area;
    }
    Self(ImplicitSkyMapArray::new(depth, densities))
  }

  pub fn to_chi2_mom(&self) -> MomVecImpl<u32, f64> {
    let chi2_merger = |depth: u8, _hash: u32, [n0, n1, n2, n3]: [&f64; 4]| -> Option<f64> {
      // With Poisson distribution:
      // * s_i = Surface of cell i = s (all cell have the same surface at a given depth)
      // * mu_i = Number of source in cell i / Surface of cell i = Density in cell i
      // * sigma_i = sqrt(Number of source in cell i) / Surface cell i = sqrt(mu_i / s)
      // weight_i = 1 / sigma_i^2 = s / mu_i
      // mu_e = weighted_mean = ( sum_{1=1}^4 weight_i * mu_i ) / ( sum_{1=1}^4 weight_i )
      //                      = 4 / ( sum_{1=1}^4 1/mu_i )
      // V_e^{-1} = s * sum_{1=1}^4 1/mu_i
      // Applying Pineau et al. 2017:
      // => sum_{1=1}^4 (mu_i - mu_e)^2 / sigma_i^2 = ... = s * [(sum_{1=1}^4 mu_i) - 4 * mu_e]

      let s = 4.0 * PI / n_hash(depth + 1) as f64;
      let one_over_s = 1.0 / s;

      let mu0 = *n0;
      let mu1 = *n1;
      let mu2 = *n2;
      let mu3 = *n3;

      let sum = mu0 + mu1 + mu2 + mu3;
      let weighted_var_inv = 1.0 / mu0.max(one_over_s)
        + 1.0 / mu1.max(one_over_s)
        + 1.0 / mu2.max(one_over_s)
        + 1.0 / mu3.max(one_over_s);
      // let weighted_var_inv = 1.0 / mu0 + 1.0 / mu1 + 1.0 / mu2 + 1.0 / mu3;
      let weighted_mean = 4.0 / weighted_var_inv;
      let chi2_of_3dof = s * (sum - 4.0 * weighted_mean);
      // chi2 3 dof:
      // 90.0% =>  6.251
      // 95.0% =>  7.815
      // 97.5% =>  9.348
      // 99.0% => 11.345
      // 99.9% => 16.266
      if chi2_of_3dof < 16.266 {
        Some(0.25 * sum)
      } else {
        None
      }
    };
    MomVecImpl::from_skymap_ref(&self.0, chi2_merger)
  }

  pub fn to_fits<W: Write>(&self, writer: W) -> Result<(), FitsError> {
    write_implicit_skymap_fits(writer, self.as_implicit_skymap_array().values.deref())
  }

  pub fn to_fits_file<P: AsRef<Path>>(&self, path: P) -> Result<(), FitsError> {
    File::create(path)
      .map_err(|e| FitsError::Io(e))
      .and_then(|file| self.to_fits(BufWriter::new(file)))
  }

  // to_png
}

#[cfg(test)]
mod tests {
  use log::debug;
  use std::fs::read_to_string;
  use std::time::SystemTime;

  use crate::nested::map::img::{
    to_mom_png_file, to_skymap_png_file, ColorMapFunctionType, PosConversion,
  };
  use crate::nested::map::skymap::{CountMap, CountMapU32, DensityMap};
  use mapproj::pseudocyl::mol::Mol;

  fn init_logger() {
    let log_level = log::LevelFilter::max();
    // let log_level = log::LevelFilter::Error;

    let _ = env_logger::builder()
      // Include all events in tests
      .filter_level(log_level)
      // Ensure events are captured by `cargo test`
      .is_test(true)
      // Ignore errors initializing the logger if tests race to configure it
      .try_init();
  }

  /*  Test only on personal computer
  #[test]
  #[cfg(not(target_arch = "wasm32"))]
  fn test_xmm_slew_dens() {
    let path = "local_resources/xmmsl3_241122_posonly.csv";
    let content = read_to_string(path).unwrap();
    let img_size = (1366, 768);
    let depth = 8;

    let it = content.lines().skip(1).map(|row| {
      let (l, b) = row.split_once(',').unwrap();
      (
        l.parse::<f64>().unwrap().to_radians(),
        b.parse::<f64>().unwrap().to_radians(),
      )
    });
    let dens_map = DensityMap::from_positions(depth, it);
    to_skymap_png_file::<'_, _, Mol, _>(
      &dens_map.0,
      img_size,
      None,
      None,
      None,
      Some(PosConversion::EqMap2GalImg),
      None,
      Some(ColorMapFunctionType::LinearLog), // LinearLog
      "local_resources/xmmsl3_241122.dens_map.png",
      false,
    )
    .unwrap();
    // println!("{:?}", dens_map);
    let dens_mom = dens_map.to_chi2_mom();
    to_mom_png_file::<'_, _, Mol, _>(
      &dens_mom,
      img_size,
      None,
      None,
      None,
      Some(PosConversion::EqMap2GalImg),
      None,
      Some(ColorMapFunctionType::LinearLog), //Some(ColorMapFunctionType::LinearSqrt)
      "local_resources/xmmsl3_241122.dens_mom.png",
      false,
    )
    .unwrap();
  }
  */

  /*  Test only on personal computer
  #[test]
  #[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
  fn test_xmm_slew_count() {
    let path = "local_resources/xmmsl3_241122_posonly.csv";
    let content = read_to_string(path).unwrap();
    let img_size = (1366, 768);
    let depth = 7;

    let it = content.lines().skip(1).map(|row| {
      let (l, b) = row.split_once(',').unwrap();
      (
        l.parse::<f64>().unwrap().to_radians(),
        b.parse::<f64>().unwrap().to_radians(),
      )
    });
    let dens_map = CountMap::from_positions(depth, it);
    to_skymap_png_file::<'_, _, Mol, _>(
      &dens_map.0,
      img_size,
      None,
      None,
      None,
      Some(PosConversion::EqMap2GalImg),
      None,
      Some(ColorMapFunctionType::LinearLog), // LinearLog
      "local_resources/xmmsl3_241122.dens_map_from_counts.png",
      false,
    )
    .unwrap();

    // println!("{:?}", dens_map);
    let dens_mom = dens_map.to_chi2_mom();
    to_mom_png_file::<'_, _, Mol, _>(
      &dens_mom,
      img_size,
      None,
      None,
      None,
      Some(PosConversion::EqMap2GalImg),
      None,
      Some(ColorMapFunctionType::LinearLog), //Some(ColorMapFunctionType::LinearSqrt)
      "local_resources/xmmsl3_241122.dens_mom_from_counts.png",
      false,
    )
    .unwrap();
  }
  */

  /* Test only on personal computer
  #[test]
  #[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
  fn test_countmap_par() {
    let n_threads = Some(4);
    let path = "./local_resources/input11.csv"; // 3.5 GB file, depth=4 chunk_size=2M => total time = 8.57 s, i.e 400 MB/s
    let depth = 6; // Test also with 10
    let has_header = false;
    let chunk_size = 2_000_000;
    // Init logger
    init_logger();
    // Build thread pool
    let mut pool_builder = rayon::ThreadPoolBuilder::new();
    if let Some(n_threads) = n_threads {
      pool_builder = pool_builder.num_threads(n_threads);
    }
    let thread_pool = pool_builder.build().unwrap();

    let tstart = SystemTime::now();
    let count_map = CountMapU32::from_csv_file_par(
      path,
      1,
      2,
      Some(','),
      has_header,
      depth,
      chunk_size,
      &thread_pool,
    )
    .unwrap();
    debug!(
      "Count map computed in {} ms",
      SystemTime::now()
        .duration_since(tstart)
        .unwrap_or_default()
        .as_millis()
    );
  }*/
}
