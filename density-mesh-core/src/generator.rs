use crate::{
    coord::Coord,
    map::DensityMap,
    mesh::{settings::GenerateDensityMeshSettings, DensityMesh, GenerateDensityMeshError},
    triangle::Triangle,
    utils::{bake_final_mesh, extrude, is_triangle_visible, triangulate},
    Scalar,
};
#[cfg(feature = "parallel")]
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

#[cfg(feature = "parallel")]
macro_rules! into_iter {
    ($v:expr) => {
        $v.into_par_iter()
    };
}

#[cfg(not(feature = "parallel"))]
macro_rules! into_iter {
    ($v:expr) => {
        $v.into_iter()
    };
}

/// Density mesh generator state object.
/// It allows you to process mesh generation in steps and track progress or cancel generation in
/// the middle of the process.
///
/// # Examples
/// ```
/// use density_mesh_core::prelude::*;
///
/// let map = DensityMap::new(2, 2, 1, vec![1, 2, 3, 1]).unwrap();
/// let settings = GenerateDensityMeshSettings {
///     points_separation: 0.5,
///     visibility_threshold: 0.0,
///     steepness_threshold: 0.0,
///     ..Default::default()
/// };
/// let mut generator = DensityMeshGenerator::new(vec![], map, settings);
/// loop {
///     match generator.process().unwrap().get_mesh_or_self() {
///         Ok(mesh) => {
///             println!("{:#?}", mesh);
///             return;
///         },
///         Err(gen) => generator = gen,
///     }
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DensityMeshGenerator {
    Uninitialized,
    FindingPoints {
        settings: GenerateDensityMeshSettings,
        map: DensityMap,
        tries: usize,
        /// [(coordinate, value, steepness)]
        remaining: Vec<(Coord, Scalar, Scalar)>,
        points: Vec<Coord>,
        progress_current: usize,
        progress_limit: usize,
    },
    Triangulate {
        settings: GenerateDensityMeshSettings,
        map: DensityMap,
        points: Vec<Coord>,
        progress_limit: usize,
    },
    Extrude {
        points: Vec<Coord>,
        triangles: Vec<Triangle>,
        size: Scalar,
        progress_limit: usize,
    },
    BakeFinalMesh {
        points: Vec<Coord>,
        triangles: Vec<Triangle>,
        progress_limit: usize,
    },
    Completed {
        mesh: DensityMesh,
        progress_limit: usize,
    },
}

impl Default for DensityMeshGenerator {
    fn default() -> Self {
        Self::Uninitialized
    }
}

impl DensityMeshGenerator {
    /// Creates new generator instance. Check struct documentation for examples.
    ///
    /// # Arguments
    /// * `points` - List of initial points.
    /// * `map` - Density map.
    /// * `settings` - Density mesh generation settings.
    ///
    /// # Returns
    /// New generator instance.
    pub fn new(
        mut points: Vec<Coord>,
        map: DensityMap,
        settings: GenerateDensityMeshSettings,
    ) -> Self {
        let scale = map.scale().max(1);
        let w = map.unscaled_width();
        let h = map.unscaled_height();
        let hc = (w as Scalar / settings.points_separation) as usize + 1;
        let vc = (h as Scalar / settings.points_separation) as usize + 1;
        if settings.is_chunk {
            points.push(Coord::new(0.0, 0.0));
            points.push(Coord::new((w - 1) as _, 0.0));
            points.push(Coord::new((w - 1) as _, (h - 1) as _));
            points.push(Coord::new(0.0, (h - 1) as _));
            for i in 1..hc {
                let v = w as Scalar * i as Scalar / hc as Scalar;
                points.push(Coord::new(v, 0.0));
                points.push(Coord::new(v, (h - 1) as _));
            }
            for i in 1..vc {
                let v = h as Scalar * i as Scalar / vc as Scalar;
                points.push(Coord::new(0.0, v));
                points.push(Coord::new((w - 1) as _, v));
            }
        }
        let remaining = map
            .value_steepness_iter()
            .filter_map(|(x, y, v, s)| {
                if v > settings.visibility_threshold && s > settings.steepness_threshold {
                    let x = (x * scale) as Scalar;
                    let y = (y * scale) as Scalar;
                    Some((Coord::new(x, y), v, s))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let progress_limit = remaining.len();
        points.reserve(progress_limit);
        let tries = settings.max_iterations;
        Self::FindingPoints {
            settings,
            map,
            tries,
            remaining,
            points,
            progress_current: 0,
            progress_limit,
        }
    }

    /// Get processing progress.
    ///
    /// # Returns
    /// `(current, limit, percentage)`
    pub fn progress(&self) -> (usize, usize, Scalar) {
        match self {
            Self::Uninitialized => (0, 0, 0.0),
            Self::FindingPoints {
                progress_current,
                progress_limit,
                ..
            } => {
                let current = *progress_limit - *progress_current;
                (
                    current,
                    *progress_limit,
                    current as Scalar / *progress_limit as Scalar,
                )
            }
            Self::Triangulate { progress_limit, .. } => (*progress_limit, *progress_limit, 1.0),
            Self::Extrude { progress_limit, .. } => (*progress_limit, *progress_limit, 1.0),
            Self::BakeFinalMesh { progress_limit, .. } => (*progress_limit, *progress_limit, 1.0),
            Self::Completed { progress_limit, .. } => (*progress_limit, *progress_limit, 1.0),
        }
    }

    /// Check if mesh generation is done.
    ///
    /// # Returns
    /// True if process is completed.
    pub fn is_done(&self) -> bool {
        match self {
            Self::Completed { .. } => true,
            _ => false,
        }
    }

    /// Tries to get inner generated mesh when ready, otherwise gets itself.
    /// This function consumes generator!
    ///
    /// # Returns
    /// Result with mesh (Ok) when completed, or self (Err) when still processing.
    ///
    /// # Examples
    /// ```
    /// use density_mesh_core::prelude::*;
    ///
    /// let map = DensityMap::new(2, 2, 1, vec![1, 2, 3, 1]).unwrap();
    /// let settings = GenerateDensityMeshSettings {
    ///     points_separation: 0.5,
    ///     visibility_threshold: 0.0,
    ///     steepness_threshold: 0.0,
    ///     ..Default::default()
    /// };
    /// let mut generator = DensityMeshGenerator::new(vec![], map, settings);
    /// match generator.get_mesh_or_self() {
    ///     Ok(mesh) => println!("{:#?}", mesh),
    ///     Err(gen) => generator = gen,
    /// }
    /// ```
    pub fn get_mesh_or_self(self) -> Result<DensityMesh, Self> {
        match self {
            Self::Completed { mesh, .. } => Ok(mesh),
            gen => Err(gen),
        }
    }

    /// Process mesh generation. Check struct documentation for examples.
    /// This function consumes generator!
    ///
    /// # Returns
    /// Result with self when processing step was successful, or error.
    pub fn process(self) -> Result<Self, GenerateDensityMeshError> {
        match self {
            Self::Uninitialized => Err(GenerateDensityMeshError::UninitializedGenerator),
            Self::FindingPoints {
                settings,
                map,
                mut tries,
                mut remaining,
                mut points,
                mut progress_current,
                progress_limit,
            } => {
                if !points.is_empty() {
                    let mds = settings.points_separation * settings.points_separation;
                    remaining = into_iter!(remaining)
                        .filter(|(p1, _, _)| {
                            points.iter().all(|p2| (*p2 - *p1).sqr_magnitude() > mds)
                        })
                        .collect::<Vec<_>>();
                    if remaining.is_empty() {
                        return Ok(Self::Triangulate {
                            settings,
                            map,
                            points,
                            progress_limit,
                        });
                    }
                }
                if let Some((point, _, _)) = remaining
                    .iter()
                    .max_by(|a, b| a.2.partial_cmp(&b.2).unwrap())
                {
                    points.push(*point);
                    tries = settings.max_iterations.max(1);
                } else if tries > 0 {
                    tries -= 1;
                } else {
                    return Ok(Self::Triangulate {
                        settings,
                        map,
                        points,
                        progress_limit,
                    });
                }
                progress_current = remaining.len();
                Ok(Self::FindingPoints {
                    settings,
                    map,
                    tries,
                    remaining,
                    points,
                    progress_current,
                    progress_limit,
                })
            }
            Self::Triangulate {
                settings,
                map,
                points,
                progress_limit,
            } => {
                let mut triangles = triangulate(&points)?;
                if !settings.keep_invisible_triangles {
                    triangles = triangles
                        .into_iter()
                        .filter(|t| {
                            is_triangle_visible(
                                points[t.a],
                                points[t.b],
                                points[t.c],
                                &map,
                                &settings,
                            )
                        })
                        .collect::<Vec<_>>();
                }
                if let Some(size) = settings.extrude_size {
                    if !settings.keep_invisible_triangles {
                        return Ok(Self::Extrude {
                            points,
                            triangles,
                            size,
                            progress_limit,
                        });
                    }
                }
                Ok(Self::BakeFinalMesh {
                    points,
                    triangles,
                    progress_limit,
                })
            }
            Self::Extrude {
                mut points,
                mut triangles,
                size,
                progress_limit,
            } => {
                let (p, t) = extrude(&points, &triangles, size);
                points.extend(p);
                triangles.extend(t);
                Ok(Self::BakeFinalMesh {
                    points,
                    triangles,
                    progress_limit,
                })
            }
            Self::BakeFinalMesh {
                points,
                triangles,
                progress_limit,
            } => Ok(Self::Completed {
                mesh: bake_final_mesh(points, triangles),
                progress_limit,
            }),
            Self::Completed { mesh, .. } => Err(GenerateDensityMeshError::AlreadyCompleted(mesh)),
        }
    }
}
