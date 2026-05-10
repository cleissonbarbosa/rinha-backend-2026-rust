use std::cell::RefCell;
use std::fs::File;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::Arc;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

use memmap2::Mmap;

use crate::types::DIM;

const K: usize = 5;
const TOP_C: usize = 16;
const MAX_NPROBE: usize = 24;
const MAX_CLUSTERS: usize = 8192;
const IVF_MAGIC: &[u8] = b"RIVF2026";
const Q16_SCALE: f32 = 32_767.0;
const REFINE_STEP: i32 = 128;
const REFINE_SCALE: f32 = Q16_SCALE * REFINE_STEP as f32;
const REFINE_MIN: i32 = -32_767 * REFINE_STEP;
const REFINE_MAX: i32 = 32_767 * REFINE_STEP;
const COARSE_TIE_GAP: f32 = 2048.0;
const SIMD_LANES: usize = 8;

thread_local! {
    // static SCRATCH: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
     static SCRATCH: RefCell<QueryScratch> = const { RefCell::new(QueryScratch::new()) };
}

struct QueryScratch {
    centroid_dist: Vec<f32>,
    probed_epoch: Vec<u32>,
    range_dist: Vec<f32>,
    epoch: u32,
}

impl QueryScratch {
    const fn new() -> Self {
        Self {
            centroid_dist: Vec::new(),
            probed_epoch: Vec::new(),
            range_dist: Vec::new(),
            epoch: 0,
        }
    }

    fn prepare(&mut self, n_clusters: usize, n_clusters_padded: usize) -> u32 {
        if self.centroid_dist.len() < n_clusters_padded {
            self.centroid_dist.resize(n_clusters_padded, 0.0);
        }
        if self.probed_epoch.len() < n_clusters {
            self.probed_epoch.resize(n_clusters, 0);
        }

        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.probed_epoch.fill(0);
            self.epoch = 1;
        }
        self.epoch
    }
}

#[derive(Clone)]
enum MappedFileData {
    Empty,
    Mmap(Arc<Mmap>),
}

#[derive(Clone)]
struct MappedFile {
    data: MappedFileData,
    path: PathBuf,
}

impl MappedFile {
    fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let len = file
            .metadata()
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len() as usize;
        let data = if len == 0 {
            MappedFileData::Empty
        } else {
            let mmap = unsafe { Mmap::map(&file).map_err(|e| format!("{}: {e}", path.display()))? };
            MappedFileData::Mmap(Arc::new(mmap))
        };

        Ok(Self {
            data,
            path: path.to_path_buf(),
        })
    }

    fn len(&self) -> usize {
        match &self.data {
            MappedFileData::Empty => 0,
            MappedFileData::Mmap(mmap) => mmap.len(),
        }
    }

    fn bytes(&self) -> &[u8] {
        match &self.data {
            MappedFileData::Empty => &[],
            MappedFileData::Mmap(mmap) => &mmap[..],
        }
    }

    fn full_bytes(&self) -> MappedBytes {
        MappedBytes {
            file: self.clone(),
            offset: 0,
            len: self.len(),
        }
    }

    fn typed_slice<T>(&self, offset: usize, count: usize) -> Result<MappedSlice<T>, String> {
        let byte_len = count
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| format!("{}: mapped slice too large", self.path.display()))?;
        let end = offset
            .checked_add(byte_len)
            .ok_or_else(|| format!("{}: mapped slice too large", self.path.display()))?;
        if end > self.len() {
            return Err(format!("{}: truncated mapped slice", self.path.display()));
        }

        if count > 0 {
            let ptr = unsafe { self.bytes().as_ptr().add(offset) };
            let align = std::mem::align_of::<T>();
            if align > 1 && (ptr as usize) % align != 0 {
                return Err(format!("{}: unaligned mapped slice", self.path.display()));
            }
        }

        Ok(MappedSlice {
            file: self.clone(),
            offset,
            len: count,
            _marker: PhantomData,
        })
    }
}

#[derive(Clone)]
struct MappedBytes {
    file: MappedFile,
    offset: usize,
    len: usize,
}

impl Deref for MappedBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.file.bytes()[self.offset..self.offset + self.len]
    }
}

#[derive(Clone)]
struct MappedSlice<T> {
    file: MappedFile,
    offset: usize,
    len: usize,
    _marker: PhantomData<T>,
}

impl<T> Deref for MappedSlice<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        unsafe {
            slice::from_raw_parts(
                self.file.bytes().as_ptr().add(self.offset) as *const T,
                self.len,
            )
        }
    }
}

#[derive(Clone)]
pub struct ExactIndex {
    n_vecs: usize,
    n_clusters: usize,
    nprobe: usize,
    dims: MappedSlice<i16>,
    residuals: MappedBytes,
    labels: MappedBytes,
    centroids: MappedSlice<f32>,
    centroids_soa: Vec<f32>,
    n_clusters_padded: usize,
    radii: MappedSlice<f32>,
    boundaries: MappedSlice<u32>,
}

impl ExactIndex {
    pub fn load(resources_dir: &Path) -> Result<Self, String> {
        let labels = MappedFile::open(&resources_dir.join("labels.bin"))?.full_bytes();
        let n_vecs = labels.len();
        if n_vecs == 0 {
            return Err("empty labels.bin".to_string());
        }

        let dims = map_i16_file(&resources_dir.join("vectors.bin"))?;
        if dims.len() != DIM * n_vecs {
            return Err(format!(
                "vectors.bin size mismatch: got {} i16s for {n_vecs} labels",
                dims.len()
            ));
        }

        let residuals = MappedFile::open(&resources_dir.join("residuals.bin"))?.full_bytes();
        if residuals.len() != DIM * n_vecs {
            return Err(format!(
                "residuals.bin size mismatch: got {} bytes for {n_vecs} labels",
                residuals.len()
            ));
        }

        let parsed = ParsedIvf::parse(MappedFile::open(&resources_dir.join("ivf.bin"))?, n_vecs)?;

        let n_clusters_padded = ((parsed.n_clusters + SIMD_LANES - 1) / SIMD_LANES) * SIMD_LANES;
        let mut centroids_soa = vec![0.0f32; n_clusters_padded * DIM];
        for c in 0..parsed.n_clusters {
            for d in 0..DIM {
                centroids_soa[d * n_clusters_padded + c] = parsed.centroids[c * DIM + d];
            }
        }

        Ok(Self {
            n_vecs,
            n_clusters: parsed.n_clusters,
            nprobe: parsed.nprobe,
            dims,
            residuals,
            labels,
            centroids: parsed.centroids,
            centroids_soa,
            n_clusters_padded,
            radii: parsed.radii,
            boundaries: parsed.boundaries,
        })
    }

    pub fn query(&self, query: &[f32; DIM]) -> usize {
        if self.n_vecs == 0 {
            return 5;
        }

        let mut query_q16 = [0i16; DIM];
        let mut query_coarse = [0f32; DIM];
        let mut query_refined = [0i32; DIM];
        for i in 0..DIM {
            let q16 = quant16(query[i]);
            query_q16[i] = q16 as i16;
            query_coarse[i] = q16 as f32;
            query_refined[i] = quant_refined(query[i]);
        }

        // let mut probe_dist = [f32::INFINITY; MAX_NPROBE];
        // let mut probe_idx = [0u32; MAX_NPROBE];
        // let mut centroid_dist = vec![0.0f32; self.n_clusters_padded];
        // let mut probed = vec![false; self.n_clusters];

        // #[cfg(all(target_arch = "x86_64", target_feature = "avx2", target_feature = "fma"))]
        // unsafe {
        //     self.scan_centroids_avx2(&query_coarse, &mut centroid_dist);
        // }
        // #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2", target_feature = "fma")))]
        // {
        //     self.scan_centroids_scalar(&query_coarse, &mut centroid_dist);
        // }

        // for c in 0..self.n_clusters {
        //     insert_probe(&mut probe_dist, &mut probe_idx, self.nprobe, c as u32, centroid_dist[c]);
        // }

        let mut top_dist = [f32::INFINITY; TOP_C];
        let mut top_idx = [0u32; TOP_C];

        SCRATCH.with(|scratch| {
            let mut scratch = scratch.borrow_mut();
            let epoch = scratch.prepare(self.n_clusters, self.n_clusters_padded);

            #[cfg(all(
                target_arch = "x86_64",
                target_feature = "avx2",
                target_feature = "fma"
            ))]
            unsafe {
                self.scan_centroids_avx2(
                    &query_coarse,
                    &mut scratch.centroid_dist[..self.n_clusters_padded],
                );
            }
            #[cfg(not(all(
                target_arch = "x86_64",
                target_feature = "avx2",
                target_feature = "fma"
            )))]
            {
                self.scan_centroids_scalar(
                    &query_coarse,
                    &mut scratch.centroid_dist[..self.n_clusters_padded],
                );
            }

            let mut probe_dist = [f32::INFINITY; MAX_NPROBE];
            let mut probe_idx = [0u32; MAX_NPROBE];
            for c in 0..self.n_clusters {
                insert_probe(
                    &mut probe_dist,
                    &mut probe_idx,
                    self.nprobe,
                    c as u32,
                    scratch.centroid_dist[c],
                );
            }
            for &cluster in probe_idx[..self.nprobe].iter() {
                let cluster = cluster as usize;
                // probed[cluster] = true;
                scratch.probed_epoch[cluster] = epoch;
                let start = self.boundaries[cluster] as usize;
                let end = self.boundaries[cluster + 1] as usize;
                // self.scan_range(&query_coarse, start, end, &mut top_dist, &mut top_idx, &mut scratch);
                self.scan_range(
                    &query_coarse,
                    start,
                    end,
                    &mut top_dist,
                    &mut top_idx,
                    &mut scratch.range_dist,
                );
            }

            let seed_fraud_count = count_top_frauds(&self.labels, &top_dist, &top_idx, K);
            if top_dist[K - 1].is_infinite() || (seed_fraud_count != 0 && seed_fraud_count != K) {
                let mut expanded = true;
                while expanded {
                    expanded = false;
                    let tau = top_dist[TOP_C - 1];
                    for c in 0..self.n_clusters {
                        // if probed[c] || lower_bound_sq(centroid_dist[c], self.radii[c]) >= tau {
                        if scratch.probed_epoch[c] == epoch
                            || lower_bound_sq(scratch.centroid_dist[c], self.radii[c]) >= tau
                        {
                            continue;
                        }
                        // probed[c] = true;
                        scratch.probed_epoch[c] = epoch;
                        let start = self.boundaries[c] as usize;
                        let end = self.boundaries[c + 1] as usize;
                        // self.scan_range(&query_coarse, start, end, &mut top_dist, &mut top_idx, &mut scratch);
                        self.scan_range(
                            &query_coarse,
                            start,
                            end,
                            &mut top_dist,
                            &mut top_idx,
                            &mut scratch.range_dist,
                        );
                        expanded = true;
                    }
                }
            }
        });

        if TOP_C > K && top_dist[K] - top_dist[K - 1] <= COARSE_TIE_GAP {
            return count_top_frauds(&self.labels, &top_dist, &top_idx, K);
        }

        let mut refined_dist = [i64::MAX; K];
        let mut refined_idx = [0u32; K];
        for &idx in &top_idx {
            let idx = idx as usize;
            if idx >= self.n_vecs {
                continue;
            }
            let mut dist = 0i64;
            for d in 0..DIM {
                let diff = i64::from(query_refined[d]) - i64::from(self.ref_refined(idx, d));
                dist += diff * diff;
            }
            insert_refined(&mut refined_dist, &mut refined_idx, idx as u32, dist);
        }

        refined_idx
            .iter()
            .take(K)
            .map(|&idx| usize::from(self.labels[idx as usize] != 0))
            .sum()
    }

    fn scan_range(
        &self,
        query: &[f32; DIM],
        start: usize,
        end: usize,
        top_dist: &mut [f32; TOP_C],
        top_idx: &mut [u32; TOP_C],
        scratch: &mut Vec<f32>,
    ) {
        let n = end - start;
        if n == 0 {
            return;
        }
        scratch.clear();
        scratch.resize(n, 0.0);

        // Loop swap: outer over dims (cache-friendly SoA reads), inner over indices.
        // Inner loop is 8-wide AVX2 SIMD when target_feature is enabled.
        #[cfg(all(
            target_arch = "x86_64",
            target_feature = "avx2",
            target_feature = "fma"
        ))]
        unsafe {
            self.compute_distances_avx2(query, start, n, scratch);
        }
        #[cfg(not(all(
            target_arch = "x86_64",
            target_feature = "avx2",
            target_feature = "fma"
        )))]
        {
            self.compute_distances_scalar(query, start, n, scratch);
        }

        for (i, &dist) in scratch.iter().enumerate() {
            insert_candidate(top_dist, top_idx, (start + i) as u32, dist);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_feature = "avx2",
        target_feature = "fma"
    ))]
    #[target_feature(enable = "avx2,fma")]
    unsafe fn compute_distances_avx2(
        &self,
        query: &[f32; DIM],
        start: usize,
        n: usize,
        scratch: &mut [f32],
    ) {
        let dims_ptr = self.dims.as_ptr();
        let n_vecs = self.n_vecs;
        let chunks = n / SIMD_LANES;
        let tail = n % SIMD_LANES;

        for d in 0..DIM {
            let q_d = _mm256_set1_ps(query[d]);
            let dim_base = d * n_vecs + start;
            let scratch_ptr = scratch.as_mut_ptr();

            for chunk in 0..chunks {
                let off = chunk * SIMD_LANES;
                // Load 8 i16 values, extend to 8 i32, then convert to 8 f32.
                let raw = _mm_loadu_si128(dims_ptr.add(dim_base + off) as *const __m128i);
                let v_i32 = _mm256_cvtepi16_epi32(raw);
                let v_f32 = _mm256_cvtepi32_ps(v_i32);
                let diff = _mm256_sub_ps(q_d, v_f32);
                let acc = _mm256_loadu_ps(scratch_ptr.add(off));
                let new_acc = _mm256_fmadd_ps(diff, diff, acc);
                _mm256_storeu_ps(scratch_ptr.add(off), new_acc);
            }

            if tail > 0 {
                let off = chunks * SIMD_LANES;
                let q_d = query[d];
                for i in 0..tail {
                    let v = *self.dims.get_unchecked(dim_base + off + i) as f32;
                    let diff = q_d - v;
                    *scratch.get_unchecked_mut(off + i) += diff * diff;
                }
            }
        }
    }

    #[allow(dead_code)]
    fn compute_distances_scalar(
        &self,
        query: &[f32; DIM],
        start: usize,
        n: usize,
        scratch: &mut [f32],
    ) {
        for d in 0..DIM {
            let q_d = query[d];
            let dim_base = d * self.n_vecs + start;
            for i in 0..n {
                let v = self.dims[dim_base + i] as f32;
                let diff = q_d - v;
                scratch[i] += diff * diff;
            }
        }
    }

    fn ref_refined(&self, idx: usize, dim: usize) -> i32 {
        let base = dim * self.n_vecs + idx;
        let hi = i32::from(self.dims[base]);
        let residual = i32::from(self.residuals[base] as i8);
        hi * REFINE_STEP + residual
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_feature = "avx2",
        target_feature = "fma"
    ))]
    #[target_feature(enable = "avx2,fma")]
    unsafe fn scan_centroids_avx2(&self, query: &[f32; DIM], dist_out: &mut [f32]) {
        let n_padded = self.n_clusters_padded;
        let chunks = n_padded / SIMD_LANES;
        let cs_ptr = self.centroids_soa.as_ptr();
        let dist_ptr = dist_out.as_mut_ptr();

        for chunk in 0..chunks {
            let off = chunk * SIMD_LANES;
            let mut acc = _mm256_setzero_ps();
            for d in 0..DIM {
                let q_d = _mm256_set1_ps(query[d]);
                let c_d = _mm256_loadu_ps(cs_ptr.add(d * n_padded + off));
                let diff = _mm256_sub_ps(q_d, c_d);
                acc = _mm256_fmadd_ps(diff, diff, acc);
            }
            _mm256_storeu_ps(dist_ptr.add(off), acc);
        }
    }

    #[allow(dead_code)]
    fn scan_centroids_scalar(&self, query: &[f32; DIM], dist_out: &mut [f32]) {
        for c in 0..self.n_clusters {
            let mut dist = 0.0f32;
            let base = c * DIM;
            for d in 0..DIM {
                let diff = query[d] - self.centroids[base + d];
                dist += diff * diff;
            }
            dist_out[c] = dist;
        }
    }
}

fn count_top_frauds(
    labels: &[u8],
    top_dist: &[f32; TOP_C],
    top_idx: &[u32; TOP_C],
    limit: usize,
) -> usize {
    let mut fraud_count = 0usize;
    for i in 0..limit {
        if top_dist[i].is_infinite() {
            break;
        }
        fraud_count += usize::from(labels[top_idx[i] as usize] != 0);
    }
    fraud_count
}

struct ParsedIvf {
    n_clusters: usize,
    nprobe: usize,
    centroids: MappedSlice<f32>,
    radii: MappedSlice<f32>,
    boundaries: MappedSlice<u32>,
}

impl ParsedIvf {
    fn parse(file: MappedFile, n_vecs: usize) -> Result<Self, String> {
        let bytes = file.bytes();
        if bytes.len() < IVF_MAGIC.len() + 20 || &bytes[..IVF_MAGIC.len()] != IVF_MAGIC {
            return Err("bad ivf header".to_string());
        }

        let mut off = IVF_MAGIC.len();
        let dim = read_u32(bytes, &mut off)? as usize;
        let n_clusters = read_u32(bytes, &mut off)? as usize;
        let nprobe = read_u32(bytes, &mut off)? as usize;
        let index_n = read_u32(bytes, &mut off)? as usize;
        let _reserved = read_u32(bytes, &mut off)?;

        if dim != DIM || index_n != n_vecs || n_clusters == 0 || n_clusters > MAX_CLUSTERS {
            return Err("bad ivf dimensions".to_string());
        }
        if nprobe == 0 || nprobe > MAX_NPROBE || nprobe > n_clusters {
            return Err("bad ivf nprobe".to_string());
        }

        let centroid_count = n_clusters * DIM;
        let centroids = read_mapped_slice::<f32>(&file, &mut off, centroid_count)?;
        let radii = read_mapped_slice::<f32>(&file, &mut off, n_clusters)?;
        let boundaries = read_mapped_slice::<u32>(&file, &mut off, n_clusters + 1)?;
        if off != file.len()
            || boundaries.first() != Some(&0)
            || boundaries.last() != Some(&(n_vecs as u32))
        {
            return Err("bad ivf payload".to_string());
        }

        Ok(Self {
            n_clusters,
            nprobe,
            centroids,
            radii,
            boundaries,
        })
    }
}

fn map_i16_file(path: &Path) -> Result<MappedSlice<i16>, String> {
    let file = MappedFile::open(path)?;
    let len = file.len();
    if len % 2 != 0 {
        return Err(format!("{} has odd length", path.display()));
    }

    file.typed_slice(0, len / 2)
}

fn read_u32(bytes: &[u8], off: &mut usize) -> Result<u32, String> {
    if *off + 4 > bytes.len() {
        return Err("truncated u32".to_string());
    }
    let value = u32::from_le_bytes(bytes[*off..*off + 4].try_into().unwrap());
    *off += 4;
    Ok(value)
}

fn read_mapped_slice<T>(
    file: &MappedFile,
    off: &mut usize,
    count: usize,
) -> Result<MappedSlice<T>, String> {
    let byte_len = count
        .checked_mul(std::mem::size_of::<T>())
        .ok_or_else(|| format!("{}: mapped slice too large", file.path.display()))?;
    let slice = file.typed_slice(*off, count)?;
    *off = off
        .checked_add(byte_len)
        .ok_or_else(|| format!("{}: mapped slice too large", file.path.display()))?;
    Ok(slice)
}

fn quant16(x: f32) -> i32 {
    (x * Q16_SCALE).round().clamp(-Q16_SCALE, Q16_SCALE) as i32
}

fn quant_refined(x: f32) -> i32 {
    (x * REFINE_SCALE)
        .round()
        .clamp(REFINE_MIN as f32, REFINE_MAX as f32) as i32
}

fn lower_bound_sq(centroid_sq_dist: f32, radius: f32) -> f32 {
    let centroid_dist = centroid_sq_dist.sqrt();
    if centroid_dist <= radius {
        0.0
    } else {
        let delta = centroid_dist - radius;
        delta * delta
    }
}

fn insert_candidate(top_dist: &mut [f32; TOP_C], top_idx: &mut [u32; TOP_C], idx: u32, dist: f32) {
    if dist >= top_dist[TOP_C - 1] {
        return;
    }
    let mut pos = TOP_C - 1;
    while pos > 0 && top_dist[pos - 1] > dist {
        top_dist[pos] = top_dist[pos - 1];
        top_idx[pos] = top_idx[pos - 1];
        pos -= 1;
    }
    top_dist[pos] = dist;
    top_idx[pos] = idx;
}

fn insert_probe(
    top_dist: &mut [f32; MAX_NPROBE],
    top_idx: &mut [u32; MAX_NPROBE],
    limit: usize,
    idx: u32,
    dist: f32,
) {
    if dist >= top_dist[limit - 1] {
        return;
    }
    let mut pos = limit - 1;
    while pos > 0 && top_dist[pos - 1] > dist {
        top_dist[pos] = top_dist[pos - 1];
        top_idx[pos] = top_idx[pos - 1];
        pos -= 1;
    }
    top_dist[pos] = dist;
    top_idx[pos] = idx;
}

fn insert_refined(top_dist: &mut [i64; K], top_idx: &mut [u32; K], idx: u32, dist: i64) {
    if dist >= top_dist[K - 1] {
        return;
    }
    let mut pos = K - 1;
    while pos > 0 && top_dist[pos - 1] > dist {
        top_dist[pos] = top_dist[pos - 1];
        top_idx[pos] = top_idx[pos - 1];
        pos -= 1;
    }
    top_dist[pos] = dist;
    top_idx[pos] = idx;
}
