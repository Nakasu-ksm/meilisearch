use std::mem::take;

use itertools::Itertools;
use log::debug;
use ordered_float::OrderedFloat;
use roaring::RoaringBitmap;
use rstar::RTree;

use super::{Criterion, CriterionParameters, CriterionResult};
use crate::criterion::Member;
use crate::search::criteria::{resolve_query_tree, CriteriaBuilder};
use crate::search::facet::{FacetNumberIter, FacetStringIter};
use crate::search::query_tree::Operation;
use crate::{FieldId, GeoPoint, Index, Result};

/// Threshold on the number of candidates that will make
/// the system to choose between one algorithm or another.
const CANDIDATES_THRESHOLD: u64 = 1000;

pub struct AscDesc<'t> {
    index: &'t Index,
    rtxn: &'t heed::RoTxn<'t>,
    member: Member,
    field_id: Option<FieldId>,
    is_ascending: bool,
    query_tree: Option<Operation>,
    rtree: Option<RTree<GeoPoint>>,
    candidates: Box<dyn Iterator<Item = heed::Result<RoaringBitmap>> + 't>,
    allowed_candidates: RoaringBitmap,
    bucket_candidates: RoaringBitmap,
    faceted_candidates: RoaringBitmap,
    parent: Box<dyn Criterion + 't>,
}

impl<'t> AscDesc<'t> {
    pub fn asc(
        index: &'t Index,
        rtxn: &'t heed::RoTxn,
        parent: Box<dyn Criterion + 't>,
        member: Member,
    ) -> Result<Self> {
        Self::new(index, rtxn, parent, member, true)
    }

    pub fn desc(
        index: &'t Index,
        rtxn: &'t heed::RoTxn,
        parent: Box<dyn Criterion + 't>,
        member: Member,
    ) -> Result<Self> {
        Self::new(index, rtxn, parent, member, false)
    }

    fn new(
        index: &'t Index,
        rtxn: &'t heed::RoTxn,
        parent: Box<dyn Criterion + 't>,
        member: Member,
        is_ascending: bool,
    ) -> Result<Self> {
        let fields_ids_map = index.fields_ids_map(rtxn)?;
        let field_id = member.field().and_then(|field| fields_ids_map.id(&field));
        let faceted_candidates = match field_id {
            Some(field_id) => {
                let number_faceted = index.number_faceted_documents_ids(rtxn, field_id)?;
                let string_faceted = index.string_faceted_documents_ids(rtxn, field_id)?;
                number_faceted | string_faceted
            }
            None => RoaringBitmap::default(),
        };
        let rtree = index.geo_rtree(rtxn)?;

        Ok(AscDesc {
            index,
            rtxn,
            member,
            field_id,
            is_ascending,
            query_tree: None,
            rtree,
            candidates: Box::new(std::iter::empty()),
            allowed_candidates: RoaringBitmap::new(),
            faceted_candidates,
            bucket_candidates: RoaringBitmap::new(),
            parent,
        })
    }
}

impl<'t> Criterion for AscDesc<'t> {
    #[logging_timer::time("AscDesc::{}")]
    fn next(&mut self, params: &mut CriterionParameters) -> Result<Option<CriterionResult>> {
        // remove excluded candidates when next is called, instead of doing it in the loop.
        self.allowed_candidates -= params.excluded_candidates;

        loop {
            debug!(
                "Facet {}({}) iteration",
                if self.is_ascending { "Asc" } else { "Desc" },
                self.member
            );

            match self.candidates.next().transpose()? {
                None if !self.allowed_candidates.is_empty() => {
                    return Ok(Some(CriterionResult {
                        query_tree: self.query_tree.clone(),
                        candidates: Some(take(&mut self.allowed_candidates)),
                        filtered_candidates: None,
                        bucket_candidates: Some(take(&mut self.bucket_candidates)),
                    }));
                }
                None => match self.parent.next(params)? {
                    Some(CriterionResult {
                        query_tree,
                        candidates,
                        filtered_candidates,
                        bucket_candidates,
                    }) => {
                        self.query_tree = query_tree;
                        let mut candidates = match (&self.query_tree, candidates) {
                            (_, Some(candidates)) => candidates,
                            (Some(qt), None) => {
                                let context = CriteriaBuilder::new(&self.rtxn, &self.index)?;
                                resolve_query_tree(&context, qt, params.wdcache)?
                            }
                            (None, None) => self.index.documents_ids(self.rtxn)?,
                        };

                        if let Some(filtered_candidates) = filtered_candidates {
                            candidates &= filtered_candidates;
                        }

                        match bucket_candidates {
                            Some(bucket_candidates) => self.bucket_candidates |= bucket_candidates,
                            None => self.bucket_candidates |= &candidates,
                        }

                        if candidates.is_empty() {
                            continue;
                        }

                        self.allowed_candidates = &candidates - params.excluded_candidates;

                        match &self.member {
                            Member::Field(field_name) => {
                                self.candidates = match self.field_id {
                                    Some(field_id) => facet_ordered(
                                        self.index,
                                        self.rtxn,
                                        field_id,
                                        self.is_ascending,
                                        candidates & &self.faceted_candidates,
                                    )?,
                                    None => Box::new(std::iter::empty()),
                                }
                            }
                            Member::Geo(point) => {
                                self.candidates = match &self.rtree {
                                    Some(rtree) => {
                                        // TODO: TAMO how to remove that?
                                        let rtree = Box::new(rtree.clone());
                                        let rtree = Box::leak(rtree);
                                        geo_point(rtree, candidates, point.clone())?
                                    }
                                    None => Box::new(std::iter::empty()),
                                }
                            }
                        };
                    }
                    None => return Ok(None),
                },
                Some(mut candidates) => {
                    candidates -= params.excluded_candidates;
                    self.allowed_candidates -= &candidates;
                    return Ok(Some(CriterionResult {
                        query_tree: self.query_tree.clone(),
                        candidates: Some(candidates),
                        filtered_candidates: None,
                        bucket_candidates: Some(take(&mut self.bucket_candidates)),
                    }));
                }
            }
        }
    }
}

fn geo_point<'t>(
    rtree: &'t RTree<GeoPoint>,
    candidates: RoaringBitmap,
    point: [f64; 2],
) -> Result<Box<dyn Iterator<Item = heed::Result<RoaringBitmap>> + 't>> {
    Ok(Box::new(
        rtree
            .nearest_neighbor_iter_with_distance_2(&point)
            .filter_map(move |(point, _distance)| {
                candidates.contains(point.data).then(|| point.data)
            })
            .map(|id| std::iter::once(id).collect::<RoaringBitmap>())
            .map(Ok),
    ))
}

/// Returns an iterator over groups of the given candidates in ascending or descending order.
///
/// It will either use an iterative or a recursive method on the whole facet database depending
/// on the number of candidates to rank.
fn facet_ordered<'t>(
    index: &'t Index,
    rtxn: &'t heed::RoTxn,
    field_id: FieldId,
    is_ascending: bool,
    candidates: RoaringBitmap,
) -> Result<Box<dyn Iterator<Item = heed::Result<RoaringBitmap>> + 't>> {
    if candidates.len() <= CANDIDATES_THRESHOLD {
        let number_iter = iterative_facet_number_ordered_iter(
            index,
            rtxn,
            field_id,
            is_ascending,
            candidates.clone(),
        )?;
        let string_iter =
            iterative_facet_string_ordered_iter(index, rtxn, field_id, is_ascending, candidates)?;
        Ok(Box::new(number_iter.chain(string_iter).map(Ok)) as Box<dyn Iterator<Item = _>>)
    } else {
        let facet_number_fn = if is_ascending {
            FacetNumberIter::new_reducing
        } else {
            FacetNumberIter::new_reverse_reducing
        };
        let number_iter = facet_number_fn(rtxn, index, field_id, candidates.clone())?
            .map(|res| res.map(|(_, docids)| docids));

        let facet_string_fn = if is_ascending {
            FacetStringIter::new_reducing
        } else {
            FacetStringIter::new_reverse_reducing
        };
        let string_iter = facet_string_fn(rtxn, index, field_id, candidates)?
            .map(|res| res.map(|(_, _, docids)| docids));

        Ok(Box::new(number_iter.chain(string_iter)))
    }
}

/// Fetch the whole list of candidates facet number values one by one and order them by it.
///
/// This function is fast when the amount of candidates to rank is small.
fn iterative_facet_number_ordered_iter<'t>(
    index: &'t Index,
    rtxn: &'t heed::RoTxn,
    field_id: FieldId,
    is_ascending: bool,
    candidates: RoaringBitmap,
) -> Result<impl Iterator<Item = RoaringBitmap> + 't> {
    let mut docids_values = Vec::with_capacity(candidates.len() as usize);
    for docid in candidates.iter() {
        let left = (field_id, docid, f64::MIN);
        let right = (field_id, docid, f64::MAX);
        let mut iter = index.field_id_docid_facet_f64s.range(rtxn, &(left..=right))?;
        let entry = if is_ascending { iter.next() } else { iter.last() };
        if let Some(((_, _, value), ())) = entry.transpose()? {
            docids_values.push((docid, OrderedFloat(value)));
        }
    }
    docids_values.sort_unstable_by_key(|(_, v)| *v);
    let iter = docids_values.into_iter();
    let iter = if is_ascending {
        Box::new(iter) as Box<dyn Iterator<Item = _>>
    } else {
        Box::new(iter.rev())
    };

    // The itertools GroupBy iterator doesn't provide an owned version, we are therefore
    // required to collect the result into an owned collection (a Vec).
    // https://github.com/rust-itertools/itertools/issues/499
    let vec: Vec<_> = iter
        .group_by(|(_, v)| *v)
        .into_iter()
        .map(|(_, ids)| ids.map(|(id, _)| id).collect())
        .collect();

    Ok(vec.into_iter())
}

/// Fetch the whole list of candidates facet string values one by one and order them by it.
///
/// This function is fast when the amount of candidates to rank is small.
fn iterative_facet_string_ordered_iter<'t>(
    index: &'t Index,
    rtxn: &'t heed::RoTxn,
    field_id: FieldId,
    is_ascending: bool,
    candidates: RoaringBitmap,
) -> Result<impl Iterator<Item = RoaringBitmap> + 't> {
    let mut docids_values = Vec::with_capacity(candidates.len() as usize);
    for docid in candidates.iter() {
        let left = (field_id, docid, "");
        let right = (field_id, docid.saturating_add(1), "");
        // FIXME Doing this means that it will never be possible to retrieve
        //       the document with id 2^32, not sure this is a real problem.
        let mut iter = index.field_id_docid_facet_strings.range(rtxn, &(left..right))?;
        let entry = if is_ascending { iter.next() } else { iter.last() };
        if let Some(((_, _, value), _)) = entry.transpose()? {
            docids_values.push((docid, value));
        }
    }
    docids_values.sort_unstable_by_key(|(_, v)| *v);
    let iter = docids_values.into_iter();
    let iter = if is_ascending {
        Box::new(iter) as Box<dyn Iterator<Item = _>>
    } else {
        Box::new(iter.rev())
    };

    // The itertools GroupBy iterator doesn't provide an owned version, we are therefore
    // required to collect the result into an owned collection (a Vec).
    // https://github.com/rust-itertools/itertools/issues/499
    let vec: Vec<_> = iter
        .group_by(|(_, v)| *v)
        .into_iter()
        .map(|(_, ids)| ids.map(|(id, _)| id).collect())
        .collect();

    Ok(vec.into_iter())
}
