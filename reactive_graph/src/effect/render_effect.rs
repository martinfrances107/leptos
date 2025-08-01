use crate::{
    channel::channel,
    effect::inner::EffectInner,
    graph::{
        AnySubscriber, ReactiveNode, SourceSet, Subscriber, ToAnySubscriber,
        WithObserver,
    },
    owner::Owner,
};
use futures::StreamExt;
use or_poisoned::OrPoisoned;
use std::{
    fmt::Debug,
    future::{Future, IntoFuture},
    mem,
    pin::Pin,
    sync::{Arc, RwLock, Weak},
};

/// A render effect is similar to an [`Effect`](super::Effect), but with two key differences:
/// 1. Its first run takes place immediately and synchronously: for example, if it is being used to
///    drive a user interface, it will run during rendering, not on the next tick after rendering.
///    (Hence “render effect.”)
/// 2. It is canceled when the `RenderEffect` itself is dropped, rather than being stored in the
///    reactive system and canceled when the `Owner` cleans up.
///
/// Unless you are implementing a rendering framework, or require one of these two characteristics,
/// it is unlikely you will use render effects directly.
///
/// Like an [`Effect`](super::Effect), a render effect runs only with the `effects` feature
/// enabled.
#[must_use = "A RenderEffect will be canceled when it is dropped. Creating a \
              RenderEffect that is not stored in some other data structure or \
              leaked will drop it immediately, and it will not react to \
              changes in signals it reads."]
pub struct RenderEffect<T>
where
    T: 'static,
{
    value: Arc<RwLock<Option<T>>>,
    inner: Arc<RwLock<EffectInner>>,
}

impl<T> Debug for RenderEffect<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderEffect")
            .field("inner", &Arc::as_ptr(&self.inner))
            .finish()
    }
}

impl<T> RenderEffect<T>
where
    T: 'static,
{
    /// Creates a new render effect, which immediately runs `fun`.
    pub fn new(fun: impl FnMut(Option<T>) -> T + 'static) -> Self {
        Self::new_with_value_erased(Box::new(fun), None)
    }

    /// Creates a new render effect with an initial value.
    pub fn new_with_value(
        fun: impl FnMut(Option<T>) -> T + 'static,
        initial_value: Option<T>,
    ) -> Self {
        Self::new_with_value_erased(Box::new(fun), initial_value)
    }

    /// Creates a new render effect, which immediately runs `fun`.
    pub async fn new_with_async_value(
        fun: impl FnMut(Option<T>) -> T + 'static,
        value: impl IntoFuture<Output = T> + 'static,
    ) -> Self {
        Self::new_with_async_value_erased(
            Box::new(fun),
            Box::pin(value.into_future()),
        )
        .await
    }

    fn new_with_value_erased(
        mut fun: Box<dyn FnMut(Option<T>) -> T + 'static>,
        initial_value: Option<T>,
    ) -> Self {
        // codegen optimisation:
        fn prep() -> (Owner, Arc<RwLock<EffectInner>>, crate::channel::Receiver)
        {
            let (observer, rx) = channel();
            let owner = Owner::new();
            let inner = Arc::new(RwLock::new(EffectInner {
                dirty: false,
                observer,
                sources: SourceSet::new(),
            }));
            (owner, inner, rx)
        }

        let (owner, inner, mut rx) = prep();

        let value = Arc::new(RwLock::new(None::<T>));

        #[cfg(not(feature = "effects"))]
        {
            let _ = initial_value;
            let _ = owner;
            let _ = &mut rx;
            let _ = &mut fun;
        }

        #[cfg(feature = "effects")]
        {
            let subscriber = inner.to_any_subscriber();
            *value.write().or_poisoned() = Some(
                owner.with(|| subscriber.with_observer(|| fun(initial_value))),
            );

            any_spawner::Executor::spawn_local({
                let value = Arc::clone(&value);

                async move {
                    while rx.next().await.is_some() {
                        if !owner.paused()
                            && subscriber.with_observer(|| {
                                subscriber.update_if_necessary()
                            })
                        {
                            subscriber.clear_sources(&subscriber);

                            let old_value =
                                mem::take(&mut *value.write().or_poisoned());
                            let new_value = owner.with_cleanup(|| {
                                subscriber.with_observer(|| fun(old_value))
                            });
                            *value.write().or_poisoned() = Some(new_value);
                        }
                    }
                }
            });
        }

        RenderEffect { value, inner }
    }

    async fn new_with_async_value_erased(
        mut fun: Box<dyn FnMut(Option<T>) -> T + 'static>,
        initial_value: Pin<Box<dyn Future<Output = T>>>,
    ) -> Self {
        // codegen optimisation:
        fn prep() -> (Owner, Arc<RwLock<EffectInner>>, crate::channel::Receiver)
        {
            let (observer, rx) = channel();
            let owner = Owner::new();
            let inner = Arc::new(RwLock::new(EffectInner {
                dirty: false,
                observer,
                sources: SourceSet::new(),
            }));
            (owner, inner, rx)
        }

        let (owner, inner, mut rx) = prep();

        let value = Arc::new(RwLock::new(None::<T>));

        #[cfg(not(feature = "effects"))]
        {
            drop(initial_value);
            let _ = owner;
            let _ = &mut rx;
            let _ = &mut fun;
        }

        #[cfg(feature = "effects")]
        {
            use crate::computed::ScopedFuture;

            let subscriber = inner.to_any_subscriber();

            let initial = subscriber
                .with_observer(|| ScopedFuture::new(initial_value))
                .await;
            *value.write().or_poisoned() = Some(initial);

            any_spawner::Executor::spawn_local({
                let value = Arc::clone(&value);

                async move {
                    while rx.next().await.is_some() {
                        if !owner.paused()
                            && subscriber.with_observer(|| {
                                subscriber.update_if_necessary()
                            })
                        {
                            subscriber.clear_sources(&subscriber);

                            let old_value =
                                mem::take(&mut *value.write().or_poisoned());
                            let new_value = owner.with_cleanup(|| {
                                subscriber.with_observer(|| fun(old_value))
                            });
                            *value.write().or_poisoned() = Some(new_value);
                        }
                    }
                }
            });
        }

        RenderEffect { value, inner }
    }

    /// Mutably accesses the current value.
    pub fn with_value_mut<U>(
        &self,
        fun: impl FnOnce(&mut T) -> U,
    ) -> Option<U> {
        self.value.write().or_poisoned().as_mut().map(fun)
    }

    /// Takes the current value, replacing it with `None`.
    pub fn take_value(&self) -> Option<T> {
        self.value.write().or_poisoned().take()
    }
}

impl<T> RenderEffect<T>
where
    T: Send + Sync + 'static,
{
    /// Creates a render effect that will run whether the `effects` feature is enabled or not.
    pub fn new_isomorphic(
        fun: impl FnMut(Option<T>) -> T + Send + Sync + 'static,
    ) -> Self {
        fn erased<T: Send + Sync + 'static>(
            mut fun: Box<dyn FnMut(Option<T>) -> T + Send + Sync + 'static>,
        ) -> RenderEffect<T> {
            let (observer, mut rx) = channel();
            let value = Arc::new(RwLock::new(None::<T>));
            let owner = Owner::new();
            let inner = Arc::new(RwLock::new(EffectInner {
                dirty: false,
                observer,
                sources: SourceSet::new(),
            }));

            let initial_value = owner
                .with(|| inner.to_any_subscriber().with_observer(|| fun(None)));
            *value.write().or_poisoned() = Some(initial_value);

            crate::spawn({
                let value = Arc::clone(&value);
                let subscriber = inner.to_any_subscriber();

                async move {
                    while rx.next().await.is_some() {
                        if !owner.paused()
                            && subscriber.with_observer(|| {
                                subscriber.update_if_necessary()
                            })
                        {
                            subscriber.clear_sources(&subscriber);

                            let old_value =
                                mem::take(&mut *value.write().or_poisoned());
                            let new_value = owner.with_cleanup(|| {
                                subscriber.with_observer(|| fun(old_value))
                            });
                            *value.write().or_poisoned() = Some(new_value);
                        }
                    }
                }
            });

            RenderEffect { value, inner }
        }

        erased(Box::new(fun))
    }
}

impl<T> ToAnySubscriber for RenderEffect<T> {
    fn to_any_subscriber(&self) -> AnySubscriber {
        AnySubscriber(
            Arc::as_ptr(&self.inner) as usize,
            Arc::downgrade(&self.inner) as Weak<dyn Subscriber + Send + Sync>,
        )
    }
}
