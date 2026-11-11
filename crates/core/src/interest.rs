//! A bag-of-words interest profile that keeps separate tastes separate.
//!
//! One centroid is a bad summary of a reader with two unrelated interests: the
//! average can land between them and recommend the midpoint nobody asked for. This
//! module hashes post text into a fixed vector, splits the positive examples into
//! up to [`MAX_INTERESTS`] clusters, and scores a post by its **best** cluster, not
//! by the mean of all of them.
//!
//! The vectorizer is deliberately small and deterministic. It is a profile of
//! words, not a learned semantic model, but it needs no network, no model file and
//! no database extension, and it makes the swing visible: two clusters whose
//! centroids barely overlap are treated as two interests from the start.

/// Dimensions of the hashed text vector.
pub const DIMENSIONS: usize = 192;
/// Never keep more than this many distinct interests.
pub const MAX_INTERESTS: usize = 4;
/// A candidate seed must be at least this far from every existing centroid.
pub const SPLIT_DISTANCE: f32 = 0.55;
/// Two centroids below this cosine are a swing, not a broad interest.
pub const SWING_SIMILARITY: f32 = 0.35;
/// k-means passes. Six is enough for a handful of short texts.
const ITERATIONS: usize = 6;

/// Hashes one token into a dimension and a sign.
fn bucket(token: &str) -> (usize, f32) {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in token.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let sign = if hash & 1 == 0 { 1.0 } else { -1.0 };
    ((hash >> 1) as usize % DIMENSIONS, sign)
}

/// Splits HTML-ish text into lowercase words, skipping markup and URLs.
fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_alphanumeric() && c != '#' && c != '@')
        .map(|token| token.trim_start_matches(['#', '@']))
        .filter(|token| token.len() >= 3 && !is_noise(token))
}

fn is_noise(token: &str) -> bool {
    matches!(
        token,
        "http"
            | "https"
            | "www"
            | "amp"
            | "quot"
            | "span"
            | "href"
            | "class"
            | "target"
            | "rel"
            | "nofollow"
            | "the"
            | "and"
            | "for"
            | "with"
            | "this"
            | "that"
            | "from"
            | "your"
    )
}

fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for value in vector.iter_mut() {
            *value /= norm;
        }
    }
}

/// One hashed, L2-normalised vector for a post or a cluster member.
#[must_use]
pub fn vector(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0; DIMENSIONS];
    let mut counts = std::collections::HashMap::<&str, u32>::new();
    for token in tokens(text) {
        *counts.entry(token).or_default() += 1;
    }
    for (token, count) in counts {
        let weight = 1.0 + (f32::from(u16::try_from(count).unwrap_or(u16::MAX))).ln();
        let (index, sign) = bucket(token);
        vector[index] += sign * weight;
    }
    normalize(&mut vector);
    vector
}

/// Cosine similarity of two already-normalised vectors.
#[must_use]
pub fn cosine(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

fn mean(members: &[Vec<f32>]) -> Vec<f32> {
    let mut centroid = vec![0.0; DIMENSIONS];
    for member in members {
        for (slot, value) in centroid.iter_mut().zip(member) {
            *slot += value;
        }
    }
    if !members.is_empty() {
        let count = f32::from(u16::try_from(members.len()).unwrap_or(u16::MAX));
        for value in &mut centroid {
            *value /= count;
        }
    }
    normalize(&mut centroid);
    centroid
}

/// Splits texts into up to [`MAX_INTERESTS`] non-overlapping centroids.
///
/// Seeds are chosen by farthest-point distance, so an interest that shares almost
/// nothing with the first one becomes its own seed instead of being averaged in.
/// A single seed stays a single interest; this function never invents a split.
#[must_use]
pub fn profile(texts: &[String]) -> Vec<Vec<f32>> {
    let mut vectors = texts.iter().map(|text| vector(text)).collect::<Vec<_>>();
    vectors.retain(|vector| vector.iter().any(|value| value.abs() > f32::EPSILON));
    if vectors.is_empty() {
        return Vec::new();
    }

    let mut centroids = vec![vectors[0].clone()];
    while centroids.len() < MAX_INTERESTS {
        let candidate = vectors
            .iter()
            .map(|vector| {
                let best = centroids
                    .iter()
                    .map(|centroid| cosine(vector, centroid))
                    .fold(f32::MIN, f32::max);
                (best, vector)
            })
            .min_by(|(left, _), (right, _)| left.total_cmp(right));
        let Some((distance, vector)) = candidate else {
            break;
        };
        if distance > 1.0 - SPLIT_DISTANCE {
            break;
        }
        centroids.push(vector.clone());
    }

    for _ in 0..ITERATIONS {
        let mut members: Vec<Vec<Vec<f32>>> = vec![Vec::new(); centroids.len()];
        for vector in &vectors {
            let best = centroids
                .iter()
                .enumerate()
                .max_by(|(_, left), (_, right)| {
                    cosine(vector, left).total_cmp(&cosine(vector, right))
                })
                .map_or(0, |(index, _)| index);
            members[best].push(vector.clone());
        }
        for (centroid, group) in centroids.iter_mut().zip(&members) {
            if !group.is_empty() {
                *centroid = mean(group);
            }
        }
    }
    centroids.retain(|centroid| centroid.iter().any(|value| value.abs() > f32::EPSILON));
    centroids
}

/// The highest similarity to any one interest. Empty profiles are neutral.
#[must_use]
pub fn best_similarity(profile: &[Vec<f32>], text: &str) -> f32 {
    if profile.is_empty() {
        return 0.0;
    }
    let vector = vector(text);
    profile
        .iter()
        .map(|centroid| cosine(&vector, centroid))
        .fold(0.0, f32::max)
}

/// Smallest pairwise centroid similarity. Low values mean the profile swings.
#[must_use]
pub fn least_overlap(profile: &[Vec<f32>]) -> f32 {
    let mut least = 1.0_f32;
    for (index, left) in profile.iter().enumerate() {
        for right in &profile[index + 1..] {
            least = least.min(cosine(left, right));
        }
    }
    least
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_unrelated_interests_stay_two_clusters() {
        let texts = [
            "rust borrow checker compiler traits",
            "rust async runtime compiler",
            "sourdough starter bread flour",
            "sourdough bread baking oven",
        ]
        .map(str::to_owned);
        let profile = profile(&texts);
        assert!(
            profile.len() >= 2,
            "a swing must not collapse into one centroid"
        );
        assert!(least_overlap(&profile) < SWING_SIMILARITY);
    }

    #[test]
    fn a_post_scores_against_its_best_interest_not_the_middle() {
        let texts = [
            "rust borrow checker compiler traits",
            "sourdough starter bread flour",
        ]
        .map(str::to_owned);
        let profile = profile(&texts);
        let rust = best_similarity(&profile, "rust compiler traits");
        let bread = best_similarity(&profile, "sourdough bread baking");
        let middle = best_similarity(&profile, "rust bread compiler sourdough");
        assert!(rust > middle);
        assert!(bread > middle);
    }

    #[test]
    fn one_interest_stays_one_cluster() {
        let texts = [
            "rust compiler borrow checker",
            "rust compiler async runtime",
        ]
        .map(str::to_owned);
        assert_eq!(profile(&texts).len(), 1);
    }
}
