//! 轻量 i18n（方案 1：枚举 + 查表，零额外依赖）。
//!
//! - [`Lang`]：支持的语言（目前中文 / 英文）。
//! - [`StrKey`]：所有需要翻译的 UI 字符串键（菜单项等）。
//! - [`I18n`]：按当前语言查表返回 `&'static str`，也可 `cycle()` 在中英间切换。
//!
//! 设计取舍：不放 Fluent 之类重型方案，控制条文本量很小，一个枚举 + 静态表
//! 足够，且编译期就能保证每个键都有译文（缺翻译会编译报错）。`PlayerController`
//! 持有 `I18n` 作为 UI 状态，菜单里「语言」项切换它，按钮/菜单项实时读它。

/// 支持的语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    /// 中文。
    Zh,
    /// 英文。
    En,
}

impl Lang {
    /// 在支持的语言间循环（用于菜单一键切换）。
    pub fn cycle(self) -> Self {
        match self {
            Lang::Zh => Lang::En,
            Lang::En => Lang::Zh,
        }
    }

    /// 该语言的显示名（菜单「语言」项用）。
    pub fn label(self) -> &'static str {
        match self {
            Lang::Zh => "中文",
            Lang::En => "English",
        }
    }

    /// 从系统环境自动识别语言。
    ///
    /// GPUI 不暴露 locale，故读标准环境变量 `LC_ALL` / `LC_MESSAGES` / `LANG` /
    /// `LANGUAGE`（顺序取第一个非空），解析其 `lang_territory` 前缀：
    /// - 以 `en` 开头 → 英文；
    /// - 其余（含 zh、ja、或无值）→ 中文。
    ///
    /// 这样 Linux/macOS/Windows(部分终端) 下能跟随系统语言，无需额外依赖；
    /// 识别不到时安全 fallback 到中文（默认）。用户仍可在菜单里手动覆盖。
    pub fn detect() -> Self {
        for var in ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"] {
            if let Ok(val) = std::env::var(var) {
                let val = val.split('.').next().unwrap_or(""); // 去掉 .UTF-8 之类编码后缀
                let val = val.split('_').next().unwrap_or(""); // 去掉 _CN 之类地区后缀
                let lang = val.split('-').next().unwrap_or(""); // 兼容 zh-CN 连字符
                if lang.eq_ignore_ascii_case("en") {
                    return Lang::En;
                }
                // 命中任一已知变量但非英文 → 用默认中文（不再往后找空变量）。
                if !lang.is_empty() {
                    return Lang::Zh;
                }
            }
        }
        Lang::Zh
    }
}

/// 需要翻译的 UI 字符串键。
///
/// 每个变体必须在 [`I18n::TABLE`] 里出现，否则编译期报 "missing key"。新增
/// 硬编码中文文本时，先在这里加键，再在两语言表里补译文。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StrKey {
    /// 「倍速」菜单项前缀（后接 " 1x"）。
    Speed,
    /// 「info」信息面板菜单项 / 面板标题。
    Info,
    /// 「步长」菜单项前缀（后接 " 5s"）。
    Step,
    /// 「语言」切换菜单项前缀（后接当前语言名）。
    Language,
    /// info 面板：「分辨率」标签（后接 " WxH"）。
    Resolution,
    /// info 面板：「帧率」标签（后接 " X.XX fps"）。
    Fps,
}

/// i18n 状态：当前语言 + 查表。
#[derive(Debug, Clone, Copy)]
pub struct I18n {
    lang: Lang,
}

impl Default for I18n {
    /// 默认跟随系统语言（[`Lang::detect`]）；用户可在菜单手动覆盖。
    fn default() -> Self {
        Self {
            lang: Lang::detect(),
        }
    }
}

impl I18n {
    /// 用指定语言构造。
    pub fn new(lang: Lang) -> Self {
        Self { lang }
    }

    /// 当前语言。
    pub fn lang(&self) -> Lang {
        self.lang
    }

    /// 设置语言。
    pub fn set_lang(&mut self, lang: Lang) {
        self.lang = lang;
    }

    /// 在支持的语言间循环切换。
    pub fn cycle(&mut self) {
        self.lang = self.lang.cycle();
    }

    /// 取键对应的译文（按当前语言）。
    pub fn get(&self, key: StrKey) -> &'static str {
        Self::TABLE[self.lang as usize][key as usize]
    }

    /// 双语言静态表：外层按 [`Lang`] 顺序（Zh=0, En=1），内层按 [`StrKey`] 顺序。
    /// 顺序必须与枚举变体声明一致，否则键会错位。
    const TABLE: [[&'static str; 6]; 2] = [
        // Zh
        ["倍速", "info", "步长", "语言", "分辨率", "帧率"],
        // En
        ["Speed", "Info", "Step", "Language", "Resolution", "FPS"],
    ];
}
