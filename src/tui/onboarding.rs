//! 初回認証入力（Onboarding）の状態。
//!
//! email と API token を入力させる。token は画面上マスク表示する（状態としては平文を保持し、
//! 描画時に伏せ字へ変換する）。検証は `GET /2.0/user` で行う（呼び出しは `app`/`event` 側）。
//!
//! 各フィールドはカーソル付きの1行エディタ（[`TextInput`]）で、readline 風の編集
//! （先頭/末尾移動・カーソル移動・行/語削除）に対応する。

/// 入力中のフィールド。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Email,
    Token,
}

/// カーソル付きの1行テキスト入力。
///
/// 文字はコードポイント単位（`Vec<char>`）で保持し、`cursor` は `0..=len` の文字位置。
/// マルチバイト文字でも安全に途中挿入・削除できる。
#[derive(Debug, Clone, Default)]
pub struct TextInput {
    chars: Vec<char>,
    cursor: usize,
}

impl TextInput {
    /// 文字列から生成する（カーソルは末尾に置く）。
    pub fn from_str(s: &str) -> Self {
        let chars: Vec<char> = s.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }

    /// 現在の値を文字列で返す。
    pub fn value(&self) -> String {
        self.chars.iter().collect()
    }

    /// 空か。
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// 文字数（マスク表示の長さに使う）。
    pub fn len(&self) -> usize {
        self.chars.len()
    }

    /// カーソル位置（文字インデックス、`0..=len`）。
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 表示用の文字スライス。
    pub fn chars(&self) -> &[char] {
        &self.chars
    }

    /// カーソル位置に1文字挿入し、カーソルを右へ進める。
    pub fn insert(&mut self, ch: char) {
        self.chars.insert(self.cursor, ch);
        self.cursor += 1;
    }

    /// カーソル直前の1文字を削除する（Backspace）。
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    /// カーソル位置の1文字を削除する（Delete、カーソルは動かさない）。
    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    /// カーソルを1つ左へ。
    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// カーソルを1つ右へ。
    pub fn move_right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    /// カーソルを先頭へ（Ctrl+A / Home）。
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// カーソルを末尾へ（Ctrl+E / End）。
    pub fn move_end(&mut self) {
        self.cursor = self.chars.len();
    }

    /// カーソルより前をすべて削除する（Ctrl+U）。
    pub fn kill_to_start(&mut self) {
        self.chars.drain(0..self.cursor);
        self.cursor = 0;
    }

    /// カーソルより後をすべて削除する（Ctrl+K）。
    pub fn kill_to_end(&mut self) {
        self.chars.truncate(self.cursor);
    }

    /// カーソル直前の1語を削除する（Ctrl+W）。先行する空白を飛ばしてから非空白を削除する。
    pub fn kill_word_before(&mut self) {
        let mut start = self.cursor;
        while start > 0 && self.chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !self.chars[start - 1].is_whitespace() {
            start -= 1;
        }
        self.chars.drain(start..self.cursor);
        self.cursor = start;
    }
}

/// Onboarding 画面の入力状態。
#[derive(Debug, Clone, Default)]
pub struct OnboardingState {
    pub email: TextInput,
    pub token: TextInput,
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
    /// アクティブフィールドへの可変参照。
    fn active_mut(&mut self) -> &mut TextInput {
        match self.field.0 {
            Field::Email => &mut self.email,
            Field::Token => &mut self.token,
        }
    }

    /// アクティブフィールドのカーソル位置に1文字挿入する。
    pub fn insert_char(&mut self, ch: char) {
        self.active_mut().insert(ch);
        self.error = None;
    }

    /// カーソル直前の1文字を削除する（Backspace）。
    pub fn backspace(&mut self) {
        self.active_mut().backspace();
        self.error = None;
    }

    /// カーソル位置の1文字を削除する（Delete）。
    pub fn delete(&mut self) {
        self.active_mut().delete();
        self.error = None;
    }

    /// カーソルを1つ左へ。
    pub fn move_left(&mut self) {
        self.active_mut().move_left();
    }

    /// カーソルを1つ右へ。
    pub fn move_right(&mut self) {
        self.active_mut().move_right();
    }

    /// カーソルを先頭へ。
    pub fn move_home(&mut self) {
        self.active_mut().move_home();
    }

    /// カーソルを末尾へ。
    pub fn move_end(&mut self) {
        self.active_mut().move_end();
    }

    /// カーソルより前を全削除する（Ctrl+U）。
    pub fn kill_to_start(&mut self) {
        self.active_mut().kill_to_start();
        self.error = None;
    }

    /// カーソルより後を全削除する（Ctrl+K）。
    pub fn kill_to_end(&mut self) {
        self.active_mut().kill_to_end();
        self.error = None;
    }

    /// カーソル直前の1語を削除する（Ctrl+W）。
    pub fn kill_word_before(&mut self) {
        self.active_mut().kill_word_before();
        self.error = None;
    }

    /// email と token のフォーカスを切り替える（カーソル位置は各フィールドで保持）。
    pub fn toggle_field(&mut self) {
        self.field.0 = match self.field.0 {
            Field::Email => Field::Token,
            Field::Token => Field::Email,
        };
    }

    /// 入力が検証に進める状態か（両方非空）。
    pub fn is_submittable(&self) -> bool {
        !self.email.value().trim().is_empty() && !self.token.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace_at_cursor() {
        let mut input = TextInput::from_str("abc");
        input.move_home();
        input.insert('X');
        assert_eq!(input.value(), "Xabc");
        assert_eq!(input.cursor(), 1);
        input.move_end();
        input.backspace();
        assert_eq!(input.value(), "Xab");
    }

    #[test]
    fn cursor_movement_is_bounded() {
        let mut input = TextInput::from_str("ab");
        input.move_left();
        input.move_left();
        input.move_left();
        assert_eq!(input.cursor(), 0);
        input.move_right();
        input.move_right();
        input.move_right();
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn delete_removes_char_under_cursor() {
        let mut input = TextInput::from_str("abc");
        input.move_home();
        input.delete();
        assert_eq!(input.value(), "bc");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn kill_to_start_and_end() {
        let mut input = TextInput::from_str("hello world");
        // カーソルを "world" の直前へ。
        input.move_home();
        for _ in 0..6 {
            input.move_right();
        }
        input.kill_to_start();
        assert_eq!(input.value(), "world");
        assert_eq!(input.cursor(), 0);
        input.move_end();
        input.kill_to_end();
        assert_eq!(input.value(), "world");
    }

    #[test]
    fn kill_word_before_deletes_previous_word() {
        let mut input = TextInput::from_str("foo bar baz");
        input.move_end();
        input.kill_word_before();
        assert_eq!(input.value(), "foo bar ");
        input.kill_word_before();
        assert_eq!(input.value(), "foo ");
    }

    #[test]
    fn onboarding_edits_active_field() {
        let mut state = OnboardingState::default();
        state.insert_char('a');
        state.insert_char('@');
        assert_eq!(state.email.value(), "a@");
        state.toggle_field();
        state.insert_char('t');
        assert_eq!(state.token.value(), "t");
        // Ctrl+U 相当で token 全消し。
        state.move_end();
        state.kill_to_start();
        assert!(state.token.is_empty());
    }
}
