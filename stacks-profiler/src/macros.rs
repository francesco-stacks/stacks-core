#[macro_export]
macro_rules! measure {
    // Name, Tag, Rate, Block
    ($name:literal, $tag:expr, rate: $rate:literal, $block:block) => {
        {
            let _guard = $crate::span!($name, $tag, rate: $rate);
            $block
        }
    };

    // Name, Rate, Block
    ($name:literal, rate: $rate:literal, $block:block) => {
        {
            let _guard = $crate::span!($name, rate: $rate);
            $block
        }
    };

    // Name, Tag, Block
    ($name:literal, $tag:expr, $block:block) => {
        {
            let _guard = $crate::span!($name, $tag);
            $block
        }
    };

    // Name, Block
    ($name:literal, $block:block) => {
        {
            let _guard = $crate::span!($name);
            $block
        }
    };

    // Trap (Name, Rate)
    ($name:literal, rate: $rate:literal) => {
        let _guard = $crate::span!($name, rate: $rate);
    };

    // Trap (Name)
    ($name:literal) => {
        let _guard = $crate::span!($name);
    };

    // Anonymous Block
    ($($t:tt)*) => {
        {
            let _guard = $crate::span!("scope");
            $($t)*
        }
    };
}

#[macro_export]
macro_rules! span {
    // Internal helpers

    (@get_id $name:literal) => {{
        static __PROFILER_SPAN_ID: std::sync::OnceLock<$crate::SpanId> = std::sync::OnceLock::new();
        __PROFILER_SPAN_ID.get_or_init(|| $crate::Profiler::new_span_id($name).with_context(module_path!()))
    }};

    (@begin $id:expr, $tag_opt:expr) => {{
        Some($crate::Profiler::begin_span($id, $tag_opt))
    }};

    (@should_sample $counter:ident, $rate:literal) => {{
        const __RATE: usize = $rate;
        if __RATE <= 1 {
            true
        } else {
            let __n = $counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            // Fast-path for power-of-two rates: n % rate == 0 <=> (n & (rate-1)) == 0
            if __RATE.is_power_of_two() {
                (__n & (__RATE - 1)) == 0
            } else {
                (__n % __RATE) == 0
            }
        }
    }};

    (@sampled $counter:ident, $rate:literal, $sampled_block:block) => {{
        if $crate::span!(@should_sample $counter, $rate) {
            $sampled_block
        } else {
            None
        }
    }};

    // Public forms

    // Name, Tag, Rate
    ($name:literal, $tag:expr, rate: $rate:literal) => {{
        static __PROFILER_SAMPLE_COUNTER: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);

        $crate::span!(@sampled __PROFILER_SAMPLE_COUNTER, $rate, {
            let __id = $crate::span!(@get_id $name);
            // Only convert the tag when we actually sample.
            let __tag: $crate::Tag = ::core::convert::Into::into($tag);
            $crate::span!(@begin __id, Some(__tag))
        })
    }};

    // Name, Rate
    ($name:literal, rate: $rate:literal) => {{
        static __PROFILER_SAMPLE_COUNTER: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);

        $crate::span!(@sampled __PROFILER_SAMPLE_COUNTER, $rate, {
            let __id = $crate::span!(@get_id $name);
            $crate::span!(@begin __id, None)
        })
    }};

    // Name, Tag
    ($name:literal, $tag:expr) => {{
        let __id = $crate::span!(@get_id $name);
        let __tag: $crate::Tag = ::core::convert::Into::into($tag);
        $crate::span!(@begin __id, Some(__tag))
    }};

    // Name
    ($name:literal) => {{
        let __id = $crate::span!(@get_id $name);
        $crate::span!(@begin __id, None)
    }};
}
