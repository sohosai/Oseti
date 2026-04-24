//! レイアウト設定モジュール（固定レイアウト）
//!
//! OBSライクな固定構成を管理します。
//! - 上段: Preview / Program（各1面）
//! - 下段: Inputs 4x2（計8枠）

use crate::source::SourceId;

pub const INPUT_COLS: usize = 4;
pub const INPUT_ROWS: usize = 2;

/// 固定用途のレイアウトタイプ
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutType {
    /// 単一ビュー
    Single,
    /// 4x2（8分割）
    Inputs4x2,
}

impl LayoutType {
    /// レイアウトのグリッド寸法を返す (cols, rows)
    pub fn dimensions(&self) -> (usize, usize) {
        match self {
            Self::Single => (1, 1),
            Self::Inputs4x2 => (INPUT_COLS, INPUT_ROWS),
        }
    }

    /// このレイアウトで必要なビュー数を返す
    pub fn view_count(&self) -> usize {
        let (cols, rows) = self.dimensions();
        cols * rows
    }
}

impl std::fmt::Display for LayoutType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Single => write!(f, "Single"),
            Self::Inputs4x2 => write!(f, "Inputs 4x2"),
        }
    }
}

/// 単一ビューの設定と状態
#[derive(Debug, Clone)]
pub struct ViewDescriptor {
    /// このビューに割り当てられたカメラ
    pub source_id: Option<SourceId>,
}

impl ViewDescriptor {
    /// 新しいビュー設定を作成
    pub fn new(source_id: Option<SourceId>) -> Self {
        Self { source_id }
    }
}

/// マルチビューレイアウト設定
///
/// 全体のグリッドレイアウトと各ビューの設定を管理します。
#[derive(Debug, Clone)]
pub struct LayoutConfig {
    /// 各ビューの設定
    views: Vec<ViewDescriptor>,
}

impl LayoutConfig {
    /// 新しいレイアウト設定を作成
    pub fn new(layout_type: LayoutType) -> Self {
        let view_count = layout_type.view_count();
        let views = (0..view_count).map(|_| ViewDescriptor::new(None)).collect();

        Self { views }
    }

    /// ビューの総数を取得
    pub fn view_count(&self) -> usize {
        self.views.len()
    }

    /// 指定インデックスのビュー設定を取得
    pub fn view(&self, index: usize) -> Option<&ViewDescriptor> {
        self.views.get(index)
    }

    /// 指定ビューにカメラを割り当て
    pub fn assign_source(&mut self, view_index: usize, source_id: Option<SourceId>) {
        if let Some(view) = self.views.get_mut(view_index) {
            view.source_id = source_id;
        }
    }
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self::new(LayoutType::Inputs4x2)
    }
}
