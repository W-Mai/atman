use alloc::{collections::BTreeSet, string::String, sync::Arc, vec::Vec};

#[derive(Debug)]
pub struct Env<V> {
    inner: Arc<EnvInner<V>>,
}

#[derive(Debug, Clone)]
struct EnvInner<V> {
    bindings: Vec<(String, V)>,
    parent: Option<Arc<EnvInner<V>>>,
}

impl<V> Clone for Env<V> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<V> Default for Env<V> {
    fn default() -> Self {
        Self {
            inner: Arc::new(EnvInner {
                bindings: Vec::new(),
                parent: None,
            }),
        }
    }
}

impl<V> Env<V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a child environment that inherits from this one.
    /// The child starts with no own bindings; lookups fall through to parent.
    pub fn child(&self) -> Self {
        Self {
            inner: Arc::new(EnvInner {
                bindings: Vec::new(),
                parent: Some(Arc::clone(&self.inner)),
            }),
        }
    }

    pub fn lookup(&self, name: &str) -> Option<&V> {
        let mut inner = &self.inner;
        loop {
            if let Some(v) = inner
                .bindings
                .iter()
                .rev()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v)
            {
                return Some(v);
            }
            match &inner.parent {
                Some(p) => inner = p,
                None => return None,
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        let mut chain: Vec<&EnvInner<V>> = Vec::new();
        let mut inner = &*self.inner;
        loop {
            chain.push(inner);
            match &inner.parent {
                Some(p) => inner = p,
                None => break,
            }
        }
        chain
            .into_iter()
            .flat_map(|inner| inner.bindings.iter().map(|(k, v)| (k.as_str(), v)))
    }

    /// Iterate over the bindings visible through lookup, excluding shadowed values.
    ///
    /// Bindings from the current frame precede bindings inherited from parent frames.
    /// Declaration order is preserved within each frame.
    pub fn iter_visible(&self) -> impl Iterator<Item = (&str, &V)> {
        let mut visible = Vec::new();
        let mut seen = BTreeSet::new();
        let mut inner = &*self.inner;

        loop {
            let frame_start = visible.len();
            for (name, value) in inner.bindings.iter().rev() {
                if seen.insert(name.as_str()) {
                    visible.push((name.as_str(), value));
                }
            }
            visible[frame_start..].reverse();

            match &inner.parent {
                Some(parent) => inner = parent,
                None => break,
            }
        }

        visible.into_iter()
    }
}

impl<V: Clone> Env<V> {
    pub fn bind(&mut self, name: impl Into<String>, value: V) {
        let name = name.into();
        let inner = Arc::make_mut(&mut self.inner);
        if let Some((_, current)) = inner.bindings.iter_mut().find(|(bound, _)| bound == &name) {
            *current = value;
        } else {
            inner.bindings.push((name, value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn empty_env_lookup_returns_none() {
        assert!(Env::<i32>::new().lookup("x").is_none());
    }

    #[test]
    fn bind_then_lookup_returns_value() {
        let mut env = Env::new();
        env.bind("x", 1);
        assert!(matches!(env.lookup("x"), Some(&1)));
    }

    #[test]
    fn repeated_binding_reuses_current_frame_slot() {
        let mut env = Env::new();
        for value in 0..10_000 {
            env.bind("x", value);
        }

        assert!(matches!(env.lookup("x"), Some(&9_999)));
        assert_eq!(env.iter().count(), 1);
    }

    #[test]
    fn unrelated_lookup_after_shadow_still_works() {
        let mut env = Env::new();
        env.bind("x", 1);
        env.bind("y", 3);
        env.bind("x", 2);
        assert!(matches!(env.lookup("y"), Some(&3)));
    }

    #[test]
    fn iter_yields_declaration_order() {
        let mut env = Env::new();
        env.bind("a", 1);
        env.bind("b", 2);
        env.bind("a", 3);
        let names: Vec<_> = env.iter().map(|(k, _)| k).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn visible_iteration_excludes_shadowed_parent_bindings() {
        let mut parent = Env::new();
        parent.bind("parent", 1);
        parent.bind("shared", 2);

        let mut child = parent.child();
        child.bind("shared", 3);
        child.bind("child", 4);

        let visible: Vec<_> = child
            .iter_visible()
            .map(|(name, value)| (name, *value))
            .collect();
        assert_eq!(visible, vec![("shared", 3), ("child", 4), ("parent", 1)]);

        let physical: Vec<_> = child.iter().map(|(name, value)| (name, *value)).collect();
        assert_eq!(
            physical,
            vec![("shared", 3), ("child", 4), ("parent", 1), ("shared", 2)]
        );
    }

    #[test]
    fn child_env_inherits_parent() {
        let mut parent = Env::new();
        parent.bind("x", 42);
        let child = parent.child();
        assert!(matches!(child.lookup("x"), Some(&42)));
    }

    #[test]
    fn child_env_bind_does_not_affect_parent() {
        let mut parent = Env::new();
        parent.bind("x", 1);
        let mut child = parent.child();
        child.bind("x", 99);
        assert!(matches!(parent.lookup("x"), Some(&1)));
        assert!(matches!(child.lookup("x"), Some(&99)));
    }

    #[test]
    fn clone_is_cheap() {
        let mut env = Env::new();
        env.bind("x", 1);
        let cloned = env.clone();
        // Both should see the same binding
        assert!(matches!(cloned.lookup("x"), Some(&1)));
        // Mutating original after clone should not affect clone (COW)
        env.bind("y", 2);
        assert!(cloned.lookup("y").is_none());
    }

    #[test]
    fn rebinding_preserves_cloned_snapshot() {
        let mut env = Env::new();
        env.bind("x", 1);
        let snapshot = env.clone();

        env.bind("x", 2);

        assert!(matches!(env.lookup("x"), Some(&2)));
        assert!(matches!(snapshot.lookup("x"), Some(&1)));
    }

    #[test]
    fn rebinding_drops_replaced_value_immediately() {
        use alloc::rc::Rc;
        use core::cell::Cell;

        #[derive(Clone)]
        struct DropCounter(Rc<Cell<usize>>);

        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let drops = Rc::new(Cell::new(0));
        {
            let mut env = Env::new();
            env.bind("value", DropCounter(Rc::clone(&drops)));
            env.bind("value", DropCounter(Rc::clone(&drops)));
            assert_eq!(drops.get(), 1);
            assert_eq!(env.iter().count(), 1);
        }
        assert_eq!(drops.get(), 2);
    }
}
