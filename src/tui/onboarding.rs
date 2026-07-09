//! 初回認証入力（Onboarding）の状態。
//!
//! email と API token を入力させる。token は画面上マスク表示する（状態としては平文を保持し、
//! 描画時に伏せ字へ変換する）。検証は `GET /2.0/user` で行う（呼び出しは `app`/`event` 側）。

/// 入力中のフィールド。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Email,
    Token,
}

/// Onboarding 画面の入力状態。
#[derive(Debug, Clone, Default)]
pub struct OnboardingState {
    pub email: String,
    pub token: String,
    pub field: FieldState,
    /// 直近の検証エラーメッセージ（あれば画面に表示）。
    pub error: Option<String>,
    /// `GET /2.0/user` による検証中フラグ（多重送信防止と表示に使う）。
    pub validating: bool,
}

/// `Field` の `Default` 実装用ラッパ（`Default` を `Email` にするため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldState(pub Field);

impl Default for FieldState {
    fn default() -> Self {
        FieldState(Field::Email)
    }
}

impl OnboardingState {
    /// アクティブフィールドの末尾に 1 文字追加する。
    pub fn push_char(&mut self, ch: char) {
        match self.field.0 {
            Field::Email => self.email.push(ch),
            Field::Token => self.token.push(ch),
        }
        self.error = None;
    }

    /// アクティブフィールドの末尾 1 文字を削除する。
    pub fn backspace(&mut self) {
        match self.field.0 {
            Field::Email => {
                self.email.pop();
            }
            Field::Token => {
                self.token.pop();
            }
        }
        self.error = None;
    }

    /// email と token のフォーカスを切り替える。
    pub fn toggle_field(&mut self) {
        self.field.0 = match self.field.0 {
            Field::Email => Field::Token,
            Field::Token => Field::Email,
        };
    }

    /// 入力が検証に進める状態か（両方非空）。
    pub fn is_submittable(&self) -> bool {
        !self.email.trim().is_empty() && !self.token.is_empty()
    }
}
