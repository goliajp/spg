//! v7.16.0 — `sqlx::Arguments` for SPG. The bind buffer holds
//! engine-side [`Value`]s; the per-type `Encode` impls push the
//! converted Value into the buffer at `bind` time.

use sqlx_core::arguments::Arguments;
use sqlx_core::encode::{Encode, IsNull};
use sqlx_core::error::BoxDynError;
use sqlx_core::impl_into_arguments_for_arguments;
use sqlx_core::types::Type;

use spg_embedded::Value as EngineValue;

use crate::database::Spg;
use crate::type_info::SpgTypeInfo;

/// One bound argument. Wraps the engine-side value plus its
/// fixed [`SpgTypeInfo`] so the engine's executor sees a
/// pre-coerced value (matches the engine's `Expr::Placeholder`
/// → `params[N-1]` substitution path that v6.1.1 wired up for
/// the pgwire extended-query protocol).
#[derive(Debug, Clone)]
pub struct SpgArgumentValue<'q> {
    /// Owned engine value. `'q` lifetime is unused today but
    /// reserved to match sqlx's generic shape — future borrowed-
    /// bind paths (large BYTEA / TEXT[]) can flow through
    /// without a breaking change to the Arguments contract.
    pub value: EngineValue,
    /// The type the bind site claims for this value. Empty
    /// when the bind reached the buffer via `add_unchecked`.
    pub type_info: Option<SpgTypeInfo>,
    /// Marker for the borrow lifetime.
    pub _phantom: core::marker::PhantomData<&'q ()>,
}

/// Buffer of bound arguments for one `Execute` call. Indexed
/// 0..N; PG-style `$1` resolves to slot 0.
#[derive(Debug, Clone, Default)]
pub struct SpgArguments<'q> {
    /// The bound values. `Vec<SpgArgumentValue>` so future
    /// borrowed cases stay non-breaking.
    pub values: Vec<SpgArgumentValue<'q>>,
}

impl<'q> SpgArguments<'q> {
    /// Drain the buffer into an owned `Vec<EngineValue>` shaped
    /// for [`spg_embedded::Database::execute_prepared`]. Called
    /// by the connection's execute path right before the engine
    /// dispatch.
    #[must_use]
    pub fn into_engine_values(self) -> Vec<EngineValue> {
        self.values.into_iter().map(|a| a.value).collect()
    }

    /// Convenience: number of bound args. `len()` is what sqlx-
    /// core uses to validate the placeholder count at
    /// `Arguments::len` time.
    #[must_use]
    pub fn count(&self) -> usize {
        self.values.len()
    }
}

impl<'q> Arguments<'q> for SpgArguments<'q> {
    type Database = Spg;

    fn reserve(&mut self, additional: usize, _size: usize) {
        self.values.reserve(additional);
    }

    fn add<T>(&mut self, value: T) -> Result<(), BoxDynError>
    where
        T: 'q + Encode<'q, Self::Database> + Type<Self::Database>,
    {
        // Push a placeholder slot so the per-type Encode can
        // write its IsNull / engine-Value into it, mirroring
        // sqlx-core's per-driver pattern. The Encode impls in
        // `crate::types` resolve the slot via the back of the
        // `values` vec.
        self.values.push(SpgArgumentValue {
            value: EngineValue::Null,
            type_info: Some(T::type_info()),
            _phantom: core::marker::PhantomData,
        });
        let mut buf: Vec<SpgArgumentValue<'q>> = Vec::new();
        let is_null = value.encode_by_ref(&mut buf)?;
        // The encode impl appended one new slot. Move it into
        // the position we just reserved and drop the placeholder.
        if let Some(written) = buf.pop() {
            let idx = self.values.len() - 1;
            self.values[idx] = written;
        }
        if matches!(is_null, IsNull::Yes) {
            let idx = self.values.len() - 1;
            self.values[idx].value = EngineValue::Null;
        }
        Ok(())
    }

    fn len(&self) -> usize {
        self.values.len()
    }
}

// sqlx-core's macro generates the trivial `IntoArguments` impl
// that lets `query.bind(...)` flow through `query.execute(pool)`.
impl_into_arguments_for_arguments!(SpgArguments<'q>);
