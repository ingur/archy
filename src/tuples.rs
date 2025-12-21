//! Tuple implementations for variadic parameter support.
//! Extend the invocations here to support more parameters.

use std::any::TypeId;
use std::future::Future;
use std::sync::Arc;

use crate::app::{
    AddEvents, AddResources, AddServices, App, IntoSystem, IntoSystemConfigs,
    SystemDescriptor, SystemFactory, SystemFuture, SystemSpawner,
};
use crate::{FromApp, Schedule, Service, SystemOutput};

// --- FromApp Tuples ---

macro_rules! impl_from_app_tuple {
    ($($T:ident),+) => {
        impl<$($T: FromApp),+> FromApp for ($($T,)+) {
            fn from_app(app: &App) -> Self { ($($T::from_app(app),)+) }
        }
    };
}

impl_from_app_tuple!(P1);
impl_from_app_tuple!(P1, P2);
impl_from_app_tuple!(P1, P2, P3);
impl_from_app_tuple!(P1, P2, P3, P4);
impl_from_app_tuple!(P1, P2, P3, P4, P5);
impl_from_app_tuple!(P1, P2, P3, P4, P5, P6);
impl_from_app_tuple!(P1, P2, P3, P4, P5, P6, P7);
impl_from_app_tuple!(P1, P2, P3, P4, P5, P6, P7, P8);
impl_from_app_tuple!(P1, P2, P3, P4, P5, P6, P7, P8, P9);
impl_from_app_tuple!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10);
impl_from_app_tuple!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11);
impl_from_app_tuple!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11, P12);

// --- IntoSystem Tuples ---

macro_rules! impl_into_system {
    () => {
        impl<Func, Fut, Out> IntoSystem<()> for Func
        where
            Func: Fn() -> Fut + Send + Sync + Clone + 'static,
            Fut: Future<Output = Out> + Send + 'static,
            Out: SystemOutput,
        {
            fn into_system(self) -> SystemSpawner {
                Box::new(move |_app, workers| {
                    (0..workers).map(|_| {
                        let func = self.clone();
                        Arc::new(move || {
                            let func = func.clone();
                            Box::pin(async move { func().await.into_result() }) as SystemFuture
                        }) as SystemFactory
                    }).collect()
                })
            }
        }
    };
    ($($P:ident),+) => {
        #[allow(non_snake_case)]
        impl<Func, Fut, Out, $($P),+> IntoSystem<($($P,)+)> for Func
        where
            Func: Fn($($P),+) -> Fut + Send + Sync + Clone + 'static,
            Fut: Future<Output = Out> + Send + 'static,
            Out: SystemOutput,
            $($P: FromApp),+
        {
            fn into_system(self) -> SystemSpawner {
                Box::new(move |app, workers| {
                    let ($($P,)+) = ($($P::from_app(app),)+);

                    (0..workers).map(|_| {
                        let func = self.clone();
                        $(let $P = $P.clone();)+
                        Arc::new(move || {
                            let func = func.clone();
                            $(let $P = $P.clone();)+
                            Box::pin(async move { func($($P),+).await.into_result() }) as SystemFuture
                        }) as SystemFactory
                    }).collect()
                })
            }
        }
    };
}

impl_into_system!();
impl_into_system!(P1);
impl_into_system!(P1, P2);
impl_into_system!(P1, P2, P3);
impl_into_system!(P1, P2, P3, P4);
impl_into_system!(P1, P2, P3, P4, P5);
impl_into_system!(P1, P2, P3, P4, P5, P6);
impl_into_system!(P1, P2, P3, P4, P5, P6, P7);
impl_into_system!(P1, P2, P3, P4, P5, P6, P7, P8);
impl_into_system!(P1, P2, P3, P4, P5, P6, P7, P8, P9);
impl_into_system!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10);
impl_into_system!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11);
impl_into_system!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11, P12);

// --- IntoSystemConfigs Tuples ---

macro_rules! impl_into_system_configs_tuple {
    ($(($T:ident, $M:ident)),+) => {
        impl<$($T, $M),+> IntoSystemConfigs<($($M,)+)> for ($($T,)+)
        where
            $($T: IntoSystem<$M>),+
        {
            #[allow(non_snake_case)]
            fn into_descriptors(self, schedule: Schedule) -> Vec<SystemDescriptor> {
                let ($($T,)+) = self;
                vec![
                    $(SystemDescriptor::new(TypeId::of::<$T>(), schedule, $T.into_system())),+
                ]
            }
        }
    };
}

impl_into_system_configs_tuple!((T1, M1), (T2, M2));
impl_into_system_configs_tuple!((T1, M1), (T2, M2), (T3, M3));
impl_into_system_configs_tuple!((T1, M1), (T2, M2), (T3, M3), (T4, M4));
impl_into_system_configs_tuple!((T1, M1), (T2, M2), (T3, M3), (T4, M4), (T5, M5));
impl_into_system_configs_tuple!((T1, M1), (T2, M2), (T3, M3), (T4, M4), (T5, M5), (T6, M6));
impl_into_system_configs_tuple!((T1, M1), (T2, M2), (T3, M3), (T4, M4), (T5, M5), (T6, M6), (T7, M7));
impl_into_system_configs_tuple!((T1, M1), (T2, M2), (T3, M3), (T4, M4), (T5, M5), (T6, M6), (T7, M7), (T8, M8));
impl_into_system_configs_tuple!((T1, M1), (T2, M2), (T3, M3), (T4, M4), (T5, M5), (T6, M6), (T7, M7), (T8, M8), (T9, M9));
impl_into_system_configs_tuple!((T1, M1), (T2, M2), (T3, M3), (T4, M4), (T5, M5), (T6, M6), (T7, M7), (T8, M8), (T9, M9), (T10, M10));
impl_into_system_configs_tuple!((T1, M1), (T2, M2), (T3, M3), (T4, M4), (T5, M5), (T6, M6), (T7, M7), (T8, M8), (T9, M9), (T10, M10), (T11, M11));
impl_into_system_configs_tuple!((T1, M1), (T2, M2), (T3, M3), (T4, M4), (T5, M5), (T6, M6), (T7, M7), (T8, M8), (T9, M9), (T10, M10), (T11, M11), (T12, M12));

// --- AddResources Tuples ---

macro_rules! impl_add_resources {
    ($($T:ident),+) => {
        impl<$($T: Send + Sync + 'static),+> AddResources for ($($T,)+) {
            #[allow(non_snake_case)]
            fn add_to(self, app: &mut App) {
                let ($($T,)+) = self;
                $(app.add_resource($T);)+
            }
        }
    };
}

impl_add_resources!(T1);
impl_add_resources!(T1, T2);
impl_add_resources!(T1, T2, T3);
impl_add_resources!(T1, T2, T3, T4);
impl_add_resources!(T1, T2, T3, T4, T5);
impl_add_resources!(T1, T2, T3, T4, T5, T6);
impl_add_resources!(T1, T2, T3, T4, T5, T6, T7);
impl_add_resources!(T1, T2, T3, T4, T5, T6, T7, T8);
impl_add_resources!(T1, T2, T3, T4, T5, T6, T7, T8, T9);
impl_add_resources!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10);
impl_add_resources!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11);
impl_add_resources!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12);

// --- AddServices Tuples ---

macro_rules! impl_add_services {
    ($($T:ident),+) => {
        impl<$($T: Service),+> AddServices for ($($T,)+) {
            fn add_to(app: &mut App) {
                $(app.add_service::<$T>();)+
            }
        }
    };
}

impl_add_services!(T1);
impl_add_services!(T1, T2);
impl_add_services!(T1, T2, T3);
impl_add_services!(T1, T2, T3, T4);
impl_add_services!(T1, T2, T3, T4, T5);
impl_add_services!(T1, T2, T3, T4, T5, T6);
impl_add_services!(T1, T2, T3, T4, T5, T6, T7);
impl_add_services!(T1, T2, T3, T4, T5, T6, T7, T8);
impl_add_services!(T1, T2, T3, T4, T5, T6, T7, T8, T9);
impl_add_services!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10);
impl_add_services!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11);
impl_add_services!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12);

// --- AddEvents Tuples ---

macro_rules! impl_add_events {
    ($($T:ident),+) => {
        impl<$($T: Clone + Send + 'static),+> AddEvents for ($($T,)+) {
            fn add_to(app: &mut App) {
                $(app.add_event::<$T>();)+
            }
        }
    };
}

impl_add_events!(T1);
impl_add_events!(T1, T2);
impl_add_events!(T1, T2, T3);
impl_add_events!(T1, T2, T3, T4);
impl_add_events!(T1, T2, T3, T4, T5);
impl_add_events!(T1, T2, T3, T4, T5, T6);
impl_add_events!(T1, T2, T3, T4, T5, T6, T7);
impl_add_events!(T1, T2, T3, T4, T5, T6, T7, T8);
impl_add_events!(T1, T2, T3, T4, T5, T6, T7, T8, T9);
impl_add_events!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10);
impl_add_events!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11);
impl_add_events!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12);
