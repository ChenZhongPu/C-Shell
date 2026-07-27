//! Curated ISO C library identifier index used by `%where`.
//!
//! This is deliberately normative metadata, not a scan of the host's include
//! directories. System headers commonly expose each other transitively and
//! would otherwise make a platform accident look like a portable include.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Function,
    FunctionLikeMacro,
    ObjectLikeMacro,
    TypeGenericMacro,
    Typedef,
    Type,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::FunctionLikeMacro => "function-like macro",
            Self::ObjectLikeMacro => "object-like macro",
            Self::TypeGenericMacro => "type-generic macro",
            Self::Typedef => "typedef",
            Self::Type => "type",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CStandard {
    C89,
    C95,
    C99,
    C11,
    C17,
    C23,
}

impl CStandard {
    pub fn label(self) -> &'static str {
        match self {
            Self::C89 => "C89",
            Self::C95 => "C95",
            Self::C99 => "C99",
            Self::C11 => "C11",
            Self::C17 => "C17",
            Self::C23 => "C23",
        }
    }

    pub fn previous(self) -> Option<Self> {
        match self {
            Self::C89 => None,
            Self::C95 => Some(Self::C89),
            Self::C99 => Some(Self::C95),
            Self::C11 => Some(Self::C99),
            Self::C17 => Some(Self::C11),
            Self::C23 => Some(Self::C17),
        }
    }

    /// Interpret the labels produced by toolchain probing and accepted by
    /// `--std`, including the historical `2x` spelling.
    pub fn from_mode(mode: &str) -> Option<Self> {
        let mode = mode.to_ascii_lowercase();
        if mode.contains("23") || mode.contains("2x") {
            Some(Self::C23)
        } else if mode.contains("17") || mode.contains("18") {
            Some(Self::C17)
        } else if mode.contains("11") {
            Some(Self::C11)
        } else if mode.contains("99") {
            Some(Self::C99)
        } else if mode.contains("95") || mode.contains("94") {
            Some(Self::C95)
        } else if mode.contains("89") || mode.contains("90") {
            Some(Self::C89)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Match {
    pub header: &'static str,
    pub kind: Kind,
    pub since: CStandard,
    /// The first revision in which this header no longer supplies the name.
    pub removed: Option<CStandard>,
}

pub struct Lookup {
    pub name: &'static str,
    pub matches: Vec<Match>,
    pub signature: Option<&'static str>,
    pub note: Option<&'static str>,
}

impl Lookup {
    pub fn headers(&self) -> Vec<&'static str> {
        let mut headers = Vec::new();
        for found in &self.matches {
            if !headers.contains(&found.header) {
                headers.push(found.header);
            }
        }
        headers
    }

    pub fn kinds(&self) -> Vec<Kind> {
        let mut kinds = Vec::new();
        for found in &self.matches {
            if !kinds.contains(&found.kind) {
                kinds.push(found.kind);
            }
        }
        kinds
    }

    pub fn since(&self) -> CStandard {
        self.matches
            .iter()
            .map(|found| found.since)
            .min()
            .expect("a lookup has at least one match")
    }

    pub fn removed(&self) -> Option<CStandard> {
        let first = self.matches[0].removed?;
        self.matches
            .iter()
            .all(|found| found.removed == Some(first))
            .then_some(first)
    }

    pub fn available_in(&self, standard: CStandard) -> bool {
        self.matches.iter().any(|found| {
            found.since <= standard && found.removed.is_none_or(|removed| standard < removed)
        })
    }
}

struct HeaderSymbols {
    header: &'static str,
    kind: Kind,
    since: CStandard,
    removed: Option<CStandard>,
    names: &'static [&'static str],
}

use CStandard::{C11, C23, C89, C95, C99};
use Kind::{Function, FunctionLikeMacro, ObjectLikeMacro, Type, TypeGenericMacro, Typedef};

/// Groups follow Annex B of WG14 N3096, the final public C23 working draft:
/// <https://www.open-std.org/jtc1/sc22/wg14/www/docs/n3096.pdf>.
/// The index focuses on the portable public names programmers query in
/// practice; implementation extensions and the optional bounds-checking annex
/// are intentionally absent.
static GROUPS: &[HeaderSymbols] = &[
    HeaderSymbols {
        header: "<assert.h>",
        kind: FunctionLikeMacro,
        since: C89,
        removed: None,
        names: &["assert"],
    },
    HeaderSymbols {
        header: "<assert.h>",
        kind: ObjectLikeMacro,
        since: C89,
        removed: None,
        names: &["NDEBUG"],
    },
    HeaderSymbols {
        header: "<assert.h>",
        kind: ObjectLikeMacro,
        since: C11,
        removed: Some(C23),
        names: &["static_assert"],
    },
    HeaderSymbols {
        header: "<complex.h>",
        kind: ObjectLikeMacro,
        since: C99,
        removed: None,
        names: &["complex", "imaginary", "_Complex_I", "_Imaginary_I", "I"],
    },
    HeaderSymbols {
        header: "<complex.h>",
        kind: FunctionLikeMacro,
        since: C11,
        removed: None,
        names: &["CMPLX", "CMPLXF", "CMPLXL"],
    },
    HeaderSymbols {
        header: "<complex.h>",
        kind: Function,
        since: C99,
        removed: None,
        names: &[
            "cacos", "cacosf", "cacosl", "casin", "casinf", "casinl", "catan", "catanf", "catanl",
            "ccos", "ccosf", "ccosl", "csin", "csinf", "csinl", "ctan", "ctanf", "ctanl", "cacosh",
            "cacoshf", "cacoshl", "casinh", "casinhf", "casinhl", "catanh", "catanhf", "catanhl",
            "ccosh", "ccoshf", "ccoshl", "csinh", "csinhf", "csinhl", "ctanh", "ctanhf", "ctanhl",
            "cexp", "cexpf", "cexpl", "clog", "clogf", "clogl", "cabs", "cabsf", "cabsl", "cpow",
            "cpowf", "cpowl", "csqrt", "csqrtf", "csqrtl", "carg", "cargf", "cargl", "cimag",
            "cimagf", "cimagl", "conj", "conjf", "conjl", "cproj", "cprojf", "cprojl", "creal",
            "crealf", "creall",
        ],
    },
    HeaderSymbols {
        header: "<ctype.h>",
        kind: Function,
        since: C89,
        removed: None,
        names: &[
            "isalnum", "isalpha", "iscntrl", "isdigit", "isgraph", "islower", "isprint", "ispunct",
            "isspace", "isupper", "isxdigit", "tolower", "toupper",
        ],
    },
    HeaderSymbols {
        header: "<ctype.h>",
        kind: Function,
        since: C99,
        removed: None,
        names: &["isblank"],
    },
    HeaderSymbols {
        header: "<errno.h>",
        kind: ObjectLikeMacro,
        since: C89,
        removed: None,
        names: &["errno", "EDOM", "ERANGE"],
    },
    HeaderSymbols {
        header: "<errno.h>",
        kind: ObjectLikeMacro,
        since: C95,
        removed: None,
        names: &["EILSEQ"],
    },
    HeaderSymbols {
        header: "<fenv.h>",
        kind: Typedef,
        since: C99,
        removed: None,
        names: &["fenv_t", "fexcept_t"],
    },
    HeaderSymbols {
        header: "<fenv.h>",
        kind: ObjectLikeMacro,
        since: C99,
        removed: None,
        names: &[
            "FE_DIVBYZERO",
            "FE_INEXACT",
            "FE_INVALID",
            "FE_OVERFLOW",
            "FE_UNDERFLOW",
            "FE_ALL_EXCEPT",
            "FE_DOWNWARD",
            "FE_TONEAREST",
            "FE_TOWARDZERO",
            "FE_UPWARD",
            "FE_DFL_ENV",
        ],
    },
    HeaderSymbols {
        header: "<fenv.h>",
        kind: Function,
        since: C99,
        removed: None,
        names: &[
            "feclearexcept",
            "fegetexceptflag",
            "feraiseexcept",
            "fesetexceptflag",
            "fetestexcept",
            "fegetround",
            "fesetround",
            "fegetenv",
            "feholdexcept",
            "fesetenv",
            "feupdateenv",
        ],
    },
    HeaderSymbols {
        header: "<float.h>",
        kind: ObjectLikeMacro,
        since: C89,
        removed: None,
        names: &[
            "FLT_RADIX",
            "FLT_ROUNDS",
            "FLT_MANT_DIG",
            "DBL_MANT_DIG",
            "LDBL_MANT_DIG",
            "FLT_DIG",
            "DBL_DIG",
            "LDBL_DIG",
            "FLT_MIN_EXP",
            "DBL_MIN_EXP",
            "LDBL_MIN_EXP",
            "FLT_MAX_EXP",
            "DBL_MAX_EXP",
            "LDBL_MAX_EXP",
            "FLT_MAX",
            "DBL_MAX",
            "LDBL_MAX",
            "FLT_EPSILON",
            "DBL_EPSILON",
            "LDBL_EPSILON",
            "FLT_MIN",
            "DBL_MIN",
            "LDBL_MIN",
        ],
    },
    HeaderSymbols {
        header: "<float.h>",
        kind: ObjectLikeMacro,
        since: C11,
        removed: None,
        names: &[
            "FLT_DECIMAL_DIG",
            "DBL_DECIMAL_DIG",
            "LDBL_DECIMAL_DIG",
            "FLT_TRUE_MIN",
            "DBL_TRUE_MIN",
            "LDBL_TRUE_MIN",
        ],
    },
    HeaderSymbols {
        header: "<inttypes.h>",
        kind: Typedef,
        since: C99,
        removed: None,
        names: &["imaxdiv_t"],
    },
    HeaderSymbols {
        header: "<inttypes.h>",
        kind: Function,
        since: C99,
        removed: None,
        names: &[
            "imaxabs",
            "imaxdiv",
            "strtoimax",
            "strtoumax",
            "wcstoimax",
            "wcstoumax",
        ],
    },
    HeaderSymbols {
        header: "<inttypes.h>",
        kind: ObjectLikeMacro,
        since: C99,
        removed: None,
        names: &[
            "PRId8", "PRId16", "PRId32", "PRId64", "PRIdMAX", "PRIdPTR", "PRIi8", "PRIi16",
            "PRIi32", "PRIi64", "PRIiMAX", "PRIiPTR", "PRIo8", "PRIo16", "PRIo32", "PRIo64",
            "PRIoMAX", "PRIoPTR", "PRIu8", "PRIu16", "PRIu32", "PRIu64", "PRIuMAX", "PRIuPTR",
            "PRIx8", "PRIx16", "PRIx32", "PRIx64", "PRIxMAX", "PRIxPTR", "PRIX8", "PRIX16",
            "PRIX32", "PRIX64", "PRIXMAX", "PRIXPTR", "SCNd8", "SCNd16", "SCNd32", "SCNd64",
            "SCNdMAX", "SCNdPTR", "SCNi8", "SCNi16", "SCNi32", "SCNi64", "SCNiMAX", "SCNiPTR",
            "SCNo8", "SCNo16", "SCNo32", "SCNo64", "SCNoMAX", "SCNoPTR", "SCNu8", "SCNu16",
            "SCNu32", "SCNu64", "SCNuMAX", "SCNuPTR", "SCNx8", "SCNx16", "SCNx32", "SCNx64",
            "SCNxMAX", "SCNxPTR",
        ],
    },
    HeaderSymbols {
        header: "<iso646.h>",
        kind: ObjectLikeMacro,
        since: C95,
        removed: None,
        names: &[
            "and", "and_eq", "bitand", "bitor", "compl", "not", "not_eq", "or", "or_eq", "xor",
            "xor_eq",
        ],
    },
    HeaderSymbols {
        header: "<limits.h>",
        kind: ObjectLikeMacro,
        since: C89,
        removed: None,
        names: &[
            "CHAR_BIT",
            "SCHAR_MIN",
            "SCHAR_MAX",
            "UCHAR_MAX",
            "CHAR_MIN",
            "CHAR_MAX",
            "MB_LEN_MAX",
            "SHRT_MIN",
            "SHRT_MAX",
            "USHRT_MAX",
            "INT_MIN",
            "INT_MAX",
            "UINT_MAX",
            "LONG_MIN",
            "LONG_MAX",
            "ULONG_MAX",
        ],
    },
    HeaderSymbols {
        header: "<limits.h>",
        kind: ObjectLikeMacro,
        since: C99,
        removed: None,
        names: &["LLONG_MIN", "LLONG_MAX", "ULLONG_MAX"],
    },
    HeaderSymbols {
        header: "<locale.h>",
        kind: Type,
        since: C89,
        removed: None,
        names: &["lconv"],
    },
    HeaderSymbols {
        header: "<locale.h>",
        kind: ObjectLikeMacro,
        since: C89,
        removed: None,
        names: &[
            "NULL",
            "LC_ALL",
            "LC_COLLATE",
            "LC_CTYPE",
            "LC_MONETARY",
            "LC_NUMERIC",
            "LC_TIME",
        ],
    },
    HeaderSymbols {
        header: "<locale.h>",
        kind: Function,
        since: C89,
        removed: None,
        names: &["setlocale", "localeconv"],
    },
    HeaderSymbols {
        header: "<math.h>",
        kind: ObjectLikeMacro,
        since: C99,
        removed: None,
        names: &[
            "HUGE_VALF",
            "HUGE_VAL",
            "HUGE_VALL",
            "INFINITY",
            "NAN",
            "FP_INFINITE",
            "FP_NAN",
            "FP_NORMAL",
            "FP_SUBNORMAL",
            "FP_ZERO",
            "MATH_ERRNO",
            "MATH_ERREXCEPT",
            "math_errhandling",
        ],
    },
    HeaderSymbols {
        header: "<math.h>",
        kind: FunctionLikeMacro,
        since: C99,
        removed: None,
        names: &[
            "fpclassify",
            "isfinite",
            "isinf",
            "isnan",
            "isnormal",
            "signbit",
            "isgreater",
            "isgreaterequal",
            "isless",
            "islessequal",
            "islessgreater",
            "isunordered",
        ],
    },
    HeaderSymbols {
        header: "<math.h>",
        kind: Function,
        since: C89,
        removed: None,
        names: &[
            "acos", "asin", "atan", "atan2", "cos", "sin", "tan", "cosh", "sinh", "tanh", "exp",
            "frexp", "ldexp", "log", "log10", "modf", "pow", "sqrt", "ceil", "fabs", "floor",
            "fmod",
        ],
    },
    HeaderSymbols {
        header: "<math.h>",
        kind: Function,
        since: C99,
        removed: None,
        names: &[
            "acosf",
            "acosl",
            "asinf",
            "asinl",
            "atanf",
            "atanl",
            "atan2f",
            "atan2l",
            "cosf",
            "cosl",
            "sinf",
            "sinl",
            "tanf",
            "tanl",
            "acosh",
            "acoshf",
            "acoshl",
            "asinh",
            "asinhf",
            "asinhl",
            "atanh",
            "atanhf",
            "atanhl",
            "coshf",
            "coshl",
            "sinhf",
            "sinhl",
            "tanhf",
            "tanhl",
            "expf",
            "expl",
            "exp2",
            "exp2f",
            "exp2l",
            "expm1",
            "expm1f",
            "expm1l",
            "frexpf",
            "frexpl",
            "ilogb",
            "ilogbf",
            "ilogbl",
            "ldexpf",
            "ldexpl",
            "logf",
            "logl",
            "log10f",
            "log10l",
            "log1p",
            "log1pf",
            "log1pl",
            "log2",
            "log2f",
            "log2l",
            "logb",
            "logbf",
            "logbl",
            "modff",
            "modfl",
            "scalbn",
            "scalbnf",
            "scalbnl",
            "scalbln",
            "scalblnf",
            "scalblnl",
            "cbrt",
            "cbrtf",
            "cbrtl",
            "hypot",
            "hypotf",
            "hypotl",
            "powf",
            "powl",
            "sqrtf",
            "sqrtl",
            "erf",
            "erff",
            "erfl",
            "erfc",
            "erfcf",
            "erfcl",
            "lgamma",
            "lgammaf",
            "lgammal",
            "tgamma",
            "tgammaf",
            "tgammal",
            "ceilf",
            "ceill",
            "floorf",
            "floorl",
            "nearbyint",
            "nearbyintf",
            "nearbyintl",
            "rint",
            "rintf",
            "rintl",
            "lrint",
            "lrintf",
            "lrintl",
            "llrint",
            "llrintf",
            "llrintl",
            "round",
            "roundf",
            "roundl",
            "lround",
            "lroundf",
            "lroundl",
            "llround",
            "llroundf",
            "llroundl",
            "trunc",
            "truncf",
            "truncl",
            "fmodf",
            "fmodl",
            "remainder",
            "remainderf",
            "remainderl",
            "remquo",
            "remquof",
            "remquol",
            "copysign",
            "copysignf",
            "copysignl",
            "nan",
            "nanf",
            "nanl",
            "nextafter",
            "nextafterf",
            "nextafterl",
            "nexttoward",
            "nexttowardf",
            "nexttowardl",
            "fdim",
            "fdimf",
            "fdiml",
            "fmax",
            "fmaxf",
            "fmaxl",
            "fmin",
            "fminf",
            "fminl",
            "fma",
            "fmaf",
            "fmal",
        ],
    },
    HeaderSymbols {
        header: "<setjmp.h>",
        kind: Typedef,
        since: C89,
        removed: None,
        names: &["jmp_buf"],
    },
    HeaderSymbols {
        header: "<setjmp.h>",
        kind: FunctionLikeMacro,
        since: C89,
        removed: None,
        names: &["setjmp"],
    },
    HeaderSymbols {
        header: "<setjmp.h>",
        kind: Function,
        since: C89,
        removed: None,
        names: &["longjmp"],
    },
    HeaderSymbols {
        header: "<signal.h>",
        kind: Typedef,
        since: C89,
        removed: None,
        names: &["sig_atomic_t"],
    },
    HeaderSymbols {
        header: "<signal.h>",
        kind: ObjectLikeMacro,
        since: C89,
        removed: None,
        names: &[
            "SIG_DFL", "SIG_ERR", "SIG_IGN", "SIGABRT", "SIGFPE", "SIGILL", "SIGINT", "SIGSEGV",
            "SIGTERM",
        ],
    },
    HeaderSymbols {
        header: "<signal.h>",
        kind: Function,
        since: C89,
        removed: None,
        names: &["signal", "raise"],
    },
    HeaderSymbols {
        header: "<stdalign.h>",
        kind: ObjectLikeMacro,
        since: C11,
        removed: Some(C23),
        names: &[
            "alignas",
            "alignof",
            "__alignas_is_defined",
            "__alignof_is_defined",
        ],
    },
    HeaderSymbols {
        header: "<stdarg.h>",
        kind: Typedef,
        since: C89,
        removed: None,
        names: &["va_list"],
    },
    HeaderSymbols {
        header: "<stdarg.h>",
        kind: FunctionLikeMacro,
        since: C89,
        removed: None,
        names: &["va_start", "va_arg", "va_end"],
    },
    HeaderSymbols {
        header: "<stdarg.h>",
        kind: FunctionLikeMacro,
        since: C99,
        removed: None,
        names: &["va_copy"],
    },
    HeaderSymbols {
        header: "<stdatomic.h>",
        kind: Typedef,
        since: C11,
        removed: None,
        names: &[
            "atomic_bool",
            "atomic_char",
            "atomic_schar",
            "atomic_uchar",
            "atomic_short",
            "atomic_ushort",
            "atomic_int",
            "atomic_uint",
            "atomic_long",
            "atomic_ulong",
            "atomic_llong",
            "atomic_ullong",
            "atomic_char16_t",
            "atomic_char32_t",
            "atomic_wchar_t",
            "atomic_intptr_t",
            "atomic_uintptr_t",
            "atomic_size_t",
            "atomic_ptrdiff_t",
            "atomic_flag",
            "memory_order",
        ],
    },
    HeaderSymbols {
        header: "<stdatomic.h>",
        kind: ObjectLikeMacro,
        since: C11,
        removed: None,
        names: &[
            "ATOMIC_BOOL_LOCK_FREE",
            "ATOMIC_CHAR_LOCK_FREE",
            "ATOMIC_SHORT_LOCK_FREE",
            "ATOMIC_INT_LOCK_FREE",
            "ATOMIC_LONG_LOCK_FREE",
            "ATOMIC_LLONG_LOCK_FREE",
            "ATOMIC_POINTER_LOCK_FREE",
            "ATOMIC_FLAG_INIT",
            "memory_order_relaxed",
            "memory_order_consume",
            "memory_order_acquire",
            "memory_order_release",
            "memory_order_acq_rel",
            "memory_order_seq_cst",
        ],
    },
    HeaderSymbols {
        header: "<stdatomic.h>",
        kind: TypeGenericMacro,
        since: C11,
        removed: None,
        names: &[
            "atomic_init",
            "kill_dependency",
            "atomic_is_lock_free",
            "atomic_store",
            "atomic_store_explicit",
            "atomic_load",
            "atomic_load_explicit",
            "atomic_exchange",
            "atomic_exchange_explicit",
            "atomic_compare_exchange_strong",
            "atomic_compare_exchange_strong_explicit",
            "atomic_compare_exchange_weak",
            "atomic_compare_exchange_weak_explicit",
            "atomic_fetch_add",
            "atomic_fetch_add_explicit",
            "atomic_fetch_sub",
            "atomic_fetch_sub_explicit",
            "atomic_fetch_or",
            "atomic_fetch_or_explicit",
            "atomic_fetch_xor",
            "atomic_fetch_xor_explicit",
            "atomic_fetch_and",
            "atomic_fetch_and_explicit",
        ],
    },
    HeaderSymbols {
        header: "<stdatomic.h>",
        kind: Function,
        since: C11,
        removed: None,
        names: &[
            "atomic_flag_test_and_set",
            "atomic_flag_test_and_set_explicit",
            "atomic_flag_clear",
            "atomic_flag_clear_explicit",
            "atomic_thread_fence",
            "atomic_signal_fence",
        ],
    },
    HeaderSymbols {
        header: "<stdbit.h>",
        kind: TypeGenericMacro,
        since: C23,
        removed: None,
        names: &[
            "stdc_leading_zeros",
            "stdc_leading_ones",
            "stdc_trailing_zeros",
            "stdc_trailing_ones",
            "stdc_first_leading_zero",
            "stdc_first_leading_one",
            "stdc_first_trailing_zero",
            "stdc_first_trailing_one",
            "stdc_count_zeros",
            "stdc_count_ones",
            "stdc_has_single_bit",
            "stdc_bit_width",
            "stdc_bit_floor",
            "stdc_bit_ceil",
        ],
    },
    HeaderSymbols {
        header: "<stdbool.h>",
        kind: ObjectLikeMacro,
        since: C99,
        removed: Some(C23),
        names: &["bool", "true", "false"],
    },
    HeaderSymbols {
        header: "<stdbool.h>",
        kind: ObjectLikeMacro,
        since: C99,
        removed: None,
        names: &["__bool_true_false_are_defined"],
    },
    HeaderSymbols {
        header: "<stdckdint.h>",
        kind: TypeGenericMacro,
        since: C23,
        removed: None,
        names: &["ckd_add", "ckd_sub", "ckd_mul"],
    },
    HeaderSymbols {
        header: "<stddef.h>",
        kind: Typedef,
        since: C89,
        removed: None,
        names: &["ptrdiff_t", "size_t", "wchar_t"],
    },
    HeaderSymbols {
        header: "<stddef.h>",
        kind: Typedef,
        since: C11,
        removed: None,
        names: &["max_align_t"],
    },
    HeaderSymbols {
        header: "<stddef.h>",
        kind: Typedef,
        since: C23,
        removed: None,
        names: &["nullptr_t"],
    },
    HeaderSymbols {
        header: "<stddef.h>",
        kind: ObjectLikeMacro,
        since: C89,
        removed: None,
        names: &["NULL"],
    },
    HeaderSymbols {
        header: "<stddef.h>",
        kind: FunctionLikeMacro,
        since: C89,
        removed: None,
        names: &["offsetof"],
    },
    HeaderSymbols {
        header: "<stddef.h>",
        kind: FunctionLikeMacro,
        since: C23,
        removed: None,
        names: &["unreachable"],
    },
    HeaderSymbols {
        header: "<stdint.h>",
        kind: Typedef,
        since: C99,
        removed: None,
        names: &[
            "int8_t",
            "int16_t",
            "int32_t",
            "int64_t",
            "uint8_t",
            "uint16_t",
            "uint32_t",
            "uint64_t",
            "int_least8_t",
            "int_least16_t",
            "int_least32_t",
            "int_least64_t",
            "uint_least8_t",
            "uint_least16_t",
            "uint_least32_t",
            "uint_least64_t",
            "int_fast8_t",
            "int_fast16_t",
            "int_fast32_t",
            "int_fast64_t",
            "uint_fast8_t",
            "uint_fast16_t",
            "uint_fast32_t",
            "uint_fast64_t",
            "intptr_t",
            "uintptr_t",
            "intmax_t",
            "uintmax_t",
        ],
    },
    HeaderSymbols {
        header: "<stdint.h>",
        kind: ObjectLikeMacro,
        since: C99,
        removed: None,
        names: &[
            "INT8_MIN",
            "INT16_MIN",
            "INT32_MIN",
            "INT64_MIN",
            "INT8_MAX",
            "INT16_MAX",
            "INT32_MAX",
            "INT64_MAX",
            "UINT8_MAX",
            "UINT16_MAX",
            "UINT32_MAX",
            "UINT64_MAX",
            "INTPTR_MIN",
            "INTPTR_MAX",
            "UINTPTR_MAX",
            "INTMAX_MIN",
            "INTMAX_MAX",
            "UINTMAX_MAX",
            "PTRDIFF_MIN",
            "PTRDIFF_MAX",
            "SIZE_MAX",
            "INT8_C",
            "INT16_C",
            "INT32_C",
            "INT64_C",
            "UINT8_C",
            "UINT16_C",
            "UINT32_C",
            "UINT64_C",
            "INTMAX_C",
            "UINTMAX_C",
        ],
    },
    HeaderSymbols {
        header: "<stdio.h>",
        kind: Typedef,
        since: C89,
        removed: None,
        names: &["FILE", "fpos_t", "size_t"],
    },
    HeaderSymbols {
        header: "<stdio.h>",
        kind: ObjectLikeMacro,
        since: C89,
        removed: None,
        names: &[
            "NULL",
            "_IOFBF",
            "_IOLBF",
            "_IONBF",
            "BUFSIZ",
            "EOF",
            "FOPEN_MAX",
            "FILENAME_MAX",
            "L_tmpnam",
            "SEEK_CUR",
            "SEEK_END",
            "SEEK_SET",
            "TMP_MAX",
        ],
    },
    HeaderSymbols {
        header: "<stdio.h>",
        kind: ObjectLikeMacro,
        since: C89,
        removed: None,
        names: &["stdin", "stdout", "stderr"],
    },
    HeaderSymbols {
        header: "<stdio.h>",
        kind: Function,
        since: C89,
        removed: None,
        names: &[
            "remove", "rename", "tmpfile", "tmpnam", "fclose", "fflush", "fopen", "freopen",
            "setbuf", "setvbuf", "fprintf", "fscanf", "printf", "scanf", "sprintf", "sscanf",
            "vfprintf", "vprintf", "vsprintf", "fgetc", "fgets", "fputc", "fputs", "getc",
            "getchar", "putc", "putchar", "puts", "ungetc", "fread", "fwrite", "fgetpos", "fseek",
            "fsetpos", "ftell", "rewind", "clearerr", "feof", "ferror", "perror",
        ],
    },
    HeaderSymbols {
        header: "<stdio.h>",
        kind: Function,
        since: C99,
        removed: None,
        names: &["snprintf", "vfscanf", "vscanf", "vsnprintf", "vsscanf"],
    },
    HeaderSymbols {
        header: "<stdio.h>",
        kind: Function,
        since: C89,
        removed: Some(C11),
        names: &["gets"],
    },
    HeaderSymbols {
        header: "<stdlib.h>",
        kind: Typedef,
        since: C89,
        removed: None,
        names: &["div_t", "ldiv_t", "size_t", "wchar_t"],
    },
    HeaderSymbols {
        header: "<stdlib.h>",
        kind: Typedef,
        since: C99,
        removed: None,
        names: &["lldiv_t"],
    },
    HeaderSymbols {
        header: "<stdlib.h>",
        kind: ObjectLikeMacro,
        since: C89,
        removed: None,
        names: &[
            "NULL",
            "EXIT_FAILURE",
            "EXIT_SUCCESS",
            "RAND_MAX",
            "MB_CUR_MAX",
        ],
    },
    HeaderSymbols {
        header: "<stdlib.h>",
        kind: Function,
        since: C89,
        removed: None,
        names: &[
            "atof", "atoi", "atol", "strtod", "strtol", "strtoul", "rand", "srand", "calloc",
            "free", "malloc", "realloc", "abort", "atexit", "exit", "getenv", "system", "bsearch",
            "qsort", "abs", "div", "labs", "ldiv", "mblen", "mbtowc", "wctomb", "mbstowcs",
            "wcstombs",
        ],
    },
    HeaderSymbols {
        header: "<stdlib.h>",
        kind: Function,
        since: C99,
        removed: None,
        names: &[
            "atoll", "strtof", "strtold", "strtoll", "strtoull", "_Exit", "llabs", "lldiv",
        ],
    },
    HeaderSymbols {
        header: "<stdlib.h>",
        kind: Function,
        since: C11,
        removed: None,
        names: &["aligned_alloc", "at_quick_exit", "quick_exit"],
    },
    HeaderSymbols {
        header: "<stdlib.h>",
        kind: Function,
        since: C23,
        removed: None,
        names: &["free_sized", "free_aligned_sized"],
    },
    HeaderSymbols {
        header: "<stdnoreturn.h>",
        kind: ObjectLikeMacro,
        since: C11,
        removed: None,
        names: &["noreturn"],
    },
    HeaderSymbols {
        header: "<string.h>",
        kind: Typedef,
        since: C89,
        removed: None,
        names: &["size_t"],
    },
    HeaderSymbols {
        header: "<string.h>",
        kind: ObjectLikeMacro,
        since: C89,
        removed: None,
        names: &["NULL"],
    },
    HeaderSymbols {
        header: "<string.h>",
        kind: Function,
        since: C89,
        removed: None,
        names: &[
            "memchr", "memcmp", "memcpy", "memmove", "memset", "strcat", "strncat", "strchr",
            "strcmp", "strncmp", "strcoll", "strcpy", "strncpy", "strcspn", "strerror", "strlen",
            "strpbrk", "strrchr", "strspn", "strstr", "strtok", "strxfrm",
        ],
    },
    HeaderSymbols {
        header: "<string.h>",
        kind: Function,
        since: C23,
        removed: None,
        names: &["memccpy", "memset_explicit", "strdup", "strndup"],
    },
    HeaderSymbols {
        header: "<tgmath.h>",
        kind: TypeGenericMacro,
        since: C99,
        removed: None,
        names: &[
            "acos",
            "asin",
            "atan",
            "acosh",
            "asinh",
            "atanh",
            "cos",
            "sin",
            "tan",
            "cosh",
            "sinh",
            "tanh",
            "exp",
            "log",
            "pow",
            "sqrt",
            "fabs",
            "atan2",
            "cbrt",
            "ceil",
            "copysign",
            "erfc",
            "erf",
            "exp2",
            "expm1",
            "fdim",
            "floor",
            "fma",
            "fmax",
            "fmin",
            "fmod",
            "frexp",
            "hypot",
            "ilogb",
            "ldexp",
            "lgamma",
            "llrint",
            "llround",
            "log10",
            "log1p",
            "log2",
            "logb",
            "lrint",
            "lround",
            "nearbyint",
            "nextafter",
            "nexttoward",
            "remainder",
            "remquo",
            "rint",
            "round",
            "scalbln",
            "scalbn",
            "tgamma",
            "trunc",
            "carg",
            "cimag",
            "conj",
            "cproj",
            "creal",
        ],
    },
    HeaderSymbols {
        header: "<tgmath.h>",
        kind: TypeGenericMacro,
        since: C23,
        removed: None,
        names: &[
            "acospi",
            "asinpi",
            "atanpi",
            "atan2pi",
            "cospi",
            "sinpi",
            "tanpi",
            "compoundn",
            "exp10",
            "exp10m1",
            "exp2m1",
            "fmaximum",
            "fmaximum_mag",
            "fmaximum_num",
            "fmaximum_mag_num",
            "fminimum",
            "fminimum_mag",
            "fminimum_num",
            "fminimum_mag_num",
            "fromfp",
            "fromfpx",
            "llogb",
            "log10p1",
            "log2p1",
            "logp1",
            "nextdown",
            "nextup",
            "pown",
            "powr",
            "rootn",
            "roundeven",
            "rsqrt",
            "ufromfp",
            "ufromfpx",
            "fadd",
            "dadd",
            "fsub",
            "dsub",
            "fmul",
            "dmul",
            "fdiv",
            "ddiv",
            "ffma",
            "dfma",
            "fsqrt",
            "dsqrt",
        ],
    },
    HeaderSymbols {
        header: "<threads.h>",
        kind: Typedef,
        since: C11,
        removed: None,
        names: &[
            "thrd_t",
            "thrd_start_t",
            "mtx_t",
            "cnd_t",
            "tss_t",
            "tss_dtor_t",
            "once_flag",
        ],
    },
    HeaderSymbols {
        header: "<threads.h>",
        kind: ObjectLikeMacro,
        since: C11,
        removed: None,
        names: &[
            "ONCE_FLAG_INIT",
            "TSS_DTOR_ITERATIONS",
            "thrd_success",
            "thrd_timedout",
            "thrd_busy",
            "thrd_error",
            "thrd_nomem",
            "mtx_plain",
            "mtx_recursive",
            "mtx_timed",
        ],
    },
    HeaderSymbols {
        header: "<threads.h>",
        kind: ObjectLikeMacro,
        since: C11,
        removed: None,
        names: &["thread_local"],
    },
    HeaderSymbols {
        header: "<threads.h>",
        kind: Function,
        since: C11,
        removed: None,
        names: &[
            "call_once",
            "thrd_create",
            "thrd_current",
            "thrd_detach",
            "thrd_equal",
            "thrd_exit",
            "thrd_join",
            "thrd_sleep",
            "thrd_yield",
            "mtx_destroy",
            "mtx_init",
            "mtx_lock",
            "mtx_timedlock",
            "mtx_trylock",
            "mtx_unlock",
            "cnd_broadcast",
            "cnd_destroy",
            "cnd_init",
            "cnd_signal",
            "cnd_timedwait",
            "cnd_wait",
            "tss_create",
            "tss_delete",
            "tss_get",
            "tss_set",
        ],
    },
    HeaderSymbols {
        header: "<time.h>",
        kind: Typedef,
        since: C89,
        removed: None,
        names: &["clock_t", "time_t", "size_t"],
    },
    HeaderSymbols {
        header: "<time.h>",
        kind: Type,
        since: C89,
        removed: None,
        names: &["tm"],
    },
    HeaderSymbols {
        header: "<time.h>",
        kind: Type,
        since: C11,
        removed: None,
        names: &["timespec"],
    },
    HeaderSymbols {
        header: "<time.h>",
        kind: ObjectLikeMacro,
        since: C89,
        removed: None,
        names: &["NULL", "CLOCKS_PER_SEC"],
    },
    HeaderSymbols {
        header: "<time.h>",
        kind: ObjectLikeMacro,
        since: C11,
        removed: None,
        names: &["TIME_UTC"],
    },
    HeaderSymbols {
        header: "<time.h>",
        kind: Function,
        since: C89,
        removed: None,
        names: &[
            "clock",
            "difftime",
            "mktime",
            "time",
            "asctime",
            "ctime",
            "gmtime",
            "localtime",
            "strftime",
        ],
    },
    HeaderSymbols {
        header: "<time.h>",
        kind: Function,
        since: C11,
        removed: None,
        names: &["timespec_get"],
    },
    HeaderSymbols {
        header: "<time.h>",
        kind: Function,
        since: C23,
        removed: None,
        names: &["timegm", "timespec_getres"],
    },
    HeaderSymbols {
        header: "<uchar.h>",
        kind: Typedef,
        since: C11,
        removed: None,
        names: &["mbstate_t", "size_t", "char16_t", "char32_t"],
    },
    HeaderSymbols {
        header: "<uchar.h>",
        kind: Typedef,
        since: C23,
        removed: None,
        names: &["char8_t"],
    },
    HeaderSymbols {
        header: "<uchar.h>",
        kind: Function,
        since: C11,
        removed: None,
        names: &["mbrtoc16", "c16rtomb", "mbrtoc32", "c32rtomb"],
    },
    HeaderSymbols {
        header: "<uchar.h>",
        kind: Function,
        since: C23,
        removed: None,
        names: &["mbrtoc8", "c8rtomb"],
    },
    HeaderSymbols {
        header: "<wchar.h>",
        kind: Typedef,
        since: C95,
        removed: None,
        names: &["wchar_t", "size_t", "mbstate_t", "wint_t"],
    },
    HeaderSymbols {
        header: "<wchar.h>",
        kind: Type,
        since: C95,
        removed: None,
        names: &["tm"],
    },
    HeaderSymbols {
        header: "<wchar.h>",
        kind: ObjectLikeMacro,
        since: C95,
        removed: None,
        names: &["NULL", "WCHAR_MIN", "WCHAR_MAX", "WEOF"],
    },
    HeaderSymbols {
        header: "<wchar.h>",
        kind: Function,
        since: C95,
        removed: None,
        names: &[
            "fwprintf",
            "fwscanf",
            "swprintf",
            "swscanf",
            "vfwprintf",
            "vfwscanf",
            "vswprintf",
            "vswscanf",
            "vwprintf",
            "vwscanf",
            "wprintf",
            "wscanf",
            "fgetwc",
            "fgetws",
            "fputwc",
            "fputws",
            "fwide",
            "getwc",
            "getwchar",
            "putwc",
            "putwchar",
            "ungetwc",
            "wcstod",
            "wcstof",
            "wcstold",
            "wcstol",
            "wcstoll",
            "wcstoul",
            "wcstoull",
            "wcscpy",
            "wcsncpy",
            "wmemcpy",
            "wmemmove",
            "wcscat",
            "wcsncat",
            "wcscmp",
            "wcscoll",
            "wcsncmp",
            "wcsxfrm",
            "wmemcmp",
            "wcschr",
            "wcscspn",
            "wcspbrk",
            "wcsrchr",
            "wcsspn",
            "wcsstr",
            "wcstok",
            "wmemchr",
            "wcslen",
            "wmemset",
            "wcsftime",
            "btowc",
            "wctob",
            "mbsinit",
            "mbrlen",
            "mbrtowc",
            "wcrtomb",
            "mbsrtowcs",
            "wcsrtombs",
        ],
    },
    HeaderSymbols {
        header: "<wctype.h>",
        kind: Typedef,
        since: C95,
        removed: None,
        names: &["wint_t", "wctrans_t", "wctype_t"],
    },
    HeaderSymbols {
        header: "<wctype.h>",
        kind: ObjectLikeMacro,
        since: C95,
        removed: None,
        names: &["WEOF"],
    },
    HeaderSymbols {
        header: "<wctype.h>",
        kind: Function,
        since: C95,
        removed: None,
        names: &[
            "iswalnum",
            "iswalpha",
            "iswcntrl",
            "iswdigit",
            "iswgraph",
            "iswlower",
            "iswprint",
            "iswpunct",
            "iswspace",
            "iswupper",
            "iswxdigit",
            "iswctype",
            "wctype",
            "towlower",
            "towupper",
            "towctrans",
            "wctrans",
        ],
    },
    HeaderSymbols {
        header: "<wctype.h>",
        kind: Function,
        since: C99,
        removed: None,
        names: &["iswblank"],
    },
];

pub fn lookup(name: &str) -> Option<Lookup> {
    let mut canonical_name = None;
    let mut matches = Vec::new();
    for group in GROUPS {
        if let Some(&found) = group.names.iter().find(|&&candidate| candidate == name) {
            canonical_name.get_or_insert(found);
            matches.push(Match {
                header: group.header,
                kind: group.kind,
                since: group.since,
                removed: group.removed,
            });
        }
    }
    Some(Lookup {
        name: canonical_name?,
        matches,
        signature: signature(name),
        note: note(name),
    })
}

fn signature(name: &str) -> Option<&'static str> {
    Some(match name {
        "printf" => "int printf(const char *restrict format, ...)",
        "scanf" => "int scanf(const char *restrict format, ...)",
        "fopen" => "FILE *fopen(const char *restrict filename, const char *restrict mode)",
        "fclose" => "int fclose(FILE *stream)",
        "fgets" => "char *fgets(char *restrict s, int n, FILE *restrict stream)",
        "puts" => "int puts(const char *s)",
        "malloc" => "void *malloc(size_t size)",
        "calloc" => "void *calloc(size_t count, size_t size)",
        "realloc" => "void *realloc(void *ptr, size_t size)",
        "free" => "void free(void *ptr)",
        "qsort" => {
            "void qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *))"
        }
        "bsearch" => {
            "void *bsearch(const void *key, const void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *))"
        }
        "memcpy" => "void *memcpy(void *restrict s1, const void *restrict s2, size_t n)",
        "memmove" => "void *memmove(void *s1, const void *s2, size_t n)",
        "memset" => "void *memset(void *s, int c, size_t n)",
        "strlen" => "size_t strlen(const char *s)",
        "strcmp" => "int strcmp(const char *s1, const char *s2)",
        "strcpy" => "char *strcpy(char *restrict s1, const char *restrict s2)",
        "strdup" => "char *strdup(const char *s)",
        "time" => "time_t time(time_t *timer)",
        "clock" => "clock_t clock(void)",
        "signal" => "void (*signal(int sig, void (*func)(int)))(int)",
        "raise" => "int raise(int sig)",
        "gets" => "char *gets(char *s)",
        _ => return None,
    })
}

fn note(name: &str) -> Option<&'static str> {
    Some(match name {
        "gets" => "deprecated; removed from ISO C in C11 because it cannot perform bounded input",
        "bool" | "true" | "false" => {
            "provided by <stdbool.h> through C17; a language keyword in C23"
        }
        "alignas" | "alignof" => "provided by <stdalign.h> through C17; a language keyword in C23",
        "static_assert" => "provided by <assert.h> in C11/C17; a language keyword in C23",
        "__bool_true_false_are_defined" => "obsolescent in C23",
        "noreturn" => {
            "still provided by <stdnoreturn.h> in C23; prefer [[noreturn]] in new C23 code"
        }
        "isalpha" | "isdigit" | "tolower" | "toupper" => {
            "an implementation may additionally provide a macro form"
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_single_and_multi_header_identifiers() {
        let printf = lookup("printf").expect("printf");
        assert_eq!(printf.headers(), vec!["<stdio.h>"]);
        assert_eq!(printf.kinds(), vec![Function]);
        assert_eq!(printf.since(), C89);

        let size_t = lookup("size_t").expect("size_t");
        assert!(size_t.headers().contains(&"<stddef.h>"));
        assert!(size_t.headers().contains(&"<stdio.h>"));
        assert!(size_t.headers().contains(&"<stdlib.h>"));
        assert_eq!(size_t.kinds(), vec![Typedef]);

        let sqrt = lookup("sqrt").expect("sqrt");
        assert_eq!(sqrt.headers(), vec!["<math.h>", "<tgmath.h>"]);
        assert_eq!(sqrt.kinds(), vec![Function, TypeGenericMacro]);

        let stdin = lookup("stdin").expect("stdin");
        assert_eq!(stdin.kinds(), vec![ObjectLikeMacro]);
    }

    #[test]
    fn tracks_introduction_and_removal_versions() {
        let gets = lookup("gets").expect("gets");
        assert!(gets.available_in(C99));
        assert!(!gets.available_in(C11));
        assert_eq!(gets.removed(), Some(C11));

        let ckd_add = lookup("ckd_add").expect("ckd_add");
        assert!(!ckd_add.available_in(CStandard::C17));
        assert!(ckd_add.available_in(C23));

        assert!(!lookup("CMPLX").expect("CMPLX").available_in(C99));
        assert!(lookup("CMPLX").expect("CMPLX").available_in(C11));
        assert!(!lookup("iswblank").expect("iswblank").available_in(C95));
        assert!(lookup("iswblank").expect("iswblank").available_in(C99));
        assert!(!lookup("bool").expect("bool").available_in(C23));
        assert!(
            lookup("__bool_true_false_are_defined")
                .expect("stdbool compatibility macro")
                .available_in(C23)
        );
        for name in ["and", "noreturn", "thread_local"] {
            assert!(
                lookup(name)
                    .expect("C23 compatibility name")
                    .available_in(C23),
                "{name} should remain available from its standard header in C23"
            );
        }

        assert_eq!(CStandard::from_mode("gnu2x"), Some(C23));
        assert_eq!(CStandard::from_mode("c17"), Some(CStandard::C17));
    }

    #[test]
    fn contains_only_valid_unique_identifiers_per_group() {
        for (group_index, group) in GROUPS.iter().enumerate() {
            for (index, &name) in group.names.iter().enumerate() {
                assert!(is_identifier(name), "invalid index name {name}");
                assert!(
                    !group.names[..index].contains(&name),
                    "duplicate {name} in {}",
                    group.header
                );
                assert!(
                    !GROUPS[..group_index]
                        .iter()
                        .any(|earlier| earlier.header == group.header
                            && earlier.names.contains(&name)),
                    "duplicate {name} across groups for {}",
                    group.header
                );
            }
        }
        assert!(lookup("getline").is_none());
    }

    fn is_identifier(name: &str) -> bool {
        let mut chars = name.bytes();
        chars
            .next()
            .is_some_and(|c| c == b'_' || c.is_ascii_alphabetic())
            && chars.all(|c| c == b'_' || c.is_ascii_alphanumeric())
    }
}
