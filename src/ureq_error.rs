use std::fmt::Display;

pub(crate) fn into_anyhow<E: Display>(e: E) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}
