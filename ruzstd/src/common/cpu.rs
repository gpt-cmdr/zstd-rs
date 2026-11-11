//! Running the decoder's loops as the processor at hand runs them best.
//!
//! The crate is compiled for a baseline processor: on x86-64 that is one without BMI2, where a
//! shift by a variable number of bits is `shl r, cl`, three micro-operations on Intel's cores and
//! a dependency on the flags, instead of `shlx`. The decoder's loops shift by a variable count for
//! every symbol and every field of a sequence, so they are compiled a second time for BMI2 and the
//! copy is chosen when the program runs, as the reference decoder does (`DYNAMIC_BMI2`).

/// Runs `f`: where the processor has BMI1, BMI2 and LZCNT (every x86-64 processor since 2015 or
/// so), in a function compiled for them.
///
/// `f` has to be a closure marked `#[inline(always)]` that calls `#[inline(always)]` functions:
/// only what is inlined here is compiled for the features. It is told whether it is compiled for
/// BMI2, for where a different way of writing something suits the instructions there are.
///
/// Finding the features out needs `std`. Without it this runs `f` as it is, which is also what
/// happens on other architectures, and when the crate itself is compiled for the features
/// (`-C target-cpu=x86-64-v3`) both ways are the same code.
#[inline(always)]
pub(crate) fn fastest<R>(f: impl FnOnce(bool) -> R) -> R {
    #[cfg(all(feature = "std", target_arch = "x86_64"))]
    {
        #[target_feature(enable = "bmi1,bmi2,lzcnt")]
        fn bmi2<R>(f: impl FnOnce(bool) -> R) -> R {
            f(true)
        }

        if has_bmi2() {
            // SAFETY: `bmi2` is compiled for BMI1, BMI2 and LZCNT, which `has_bmi2` has just
            // found this processor to have. This is the only call of it.
            return unsafe { bmi2(f) };
        }
    }
    baseline(f)
}

/// Runs `f` as the crate is compiled. It is a function of its own, as the one for BMI2 has to
/// be: a loop that is alone in its function gets the registers, which it does not when it is
/// inlined into a caller with values of its own to keep.
#[inline(never)]
fn baseline<R>(f: impl FnOnce(bool) -> R) -> R {
    f(cfg!(target_feature = "bmi2"))
}

#[cfg(all(feature = "std", target_arch = "x86_64"))]
#[inline]
fn has_bmi2() -> bool {
    #[cfg(test)]
    if testing::PORTABLE.with(|portable| portable.get()) {
        return false;
    }
    std::is_x86_feature_detected!("bmi1")
        && std::is_x86_feature_detected!("bmi2")
        && std::is_x86_feature_detected!("lzcnt")
}

/// For the tests: both copies of a loop have to be tested on the one machine the tests run on.
#[cfg(test)]
pub(crate) mod testing {
    extern crate std;

    std::thread_local! {
        /// Whether [`fastest`](super::fastest) leaves the copies for newer processors alone on
        /// this thread.
        #[allow(dead_code)]
        pub(super) static PORTABLE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    }

    /// Runs `test` twice: with the loops chosen as the processor allows, then with only those
    /// the crate is compiled with. Where there is no second copy to choose, the two runs are the
    /// same.
    pub(crate) fn on_every_path(test: impl Fn()) {
        test();
        PORTABLE.with(|portable| portable.set(true));
        // Not set back if `test` panics: the thread of a failed test ends there.
        test();
        PORTABLE.with(|portable| portable.set(false));
    }

    /// The two runs of [`on_every_path`] are the two copies, where the processor has a choice.
    #[test]
    fn every_path_is_run() {
        let runs = core::cell::RefCell::new(alloc::vec::Vec::new());
        on_every_path(|| {
            let for_bmi2 = super::fastest(
                #[inline(always)]
                |bmi2| bmi2,
            );
            runs.borrow_mut().push(for_bmi2);
        });
        let runs = runs.into_inner();
        let compiled_in = cfg!(target_feature = "bmi2");
        #[cfg(all(feature = "std", target_arch = "x86_64"))]
        let detected = std::is_x86_feature_detected!("bmi1")
            && std::is_x86_feature_detected!("bmi2")
            && std::is_x86_feature_detected!("lzcnt");
        #[cfg(not(all(feature = "std", target_arch = "x86_64")))]
        let detected = false;
        assert_eq!(runs, [detected || compiled_in, compiled_in]);
    }
}
