use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AnnotationTool {
    Pen,
    Eraser,
    Text,
    Clear,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationStroke {
    pub tool: AnnotationTool,
    pub color: String,
    pub width: f64,
    pub points: Vec<AnnotationPoint>,
}

pub struct AnnotationEngine {
    strokes: Vec<AnnotationStroke>,
    current_tool: AnnotationTool,
    current_color: String,
    current_width: f64,
    enabled: bool,
}

impl AnnotationEngine {
    pub fn new() -> Self {
        Self {
            strokes: Vec::new(),
            current_tool: AnnotationTool::Pen,
            current_color: "#ff3b30".to_string(),
            current_width: 3.0,
            enabled: false,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_tool(&mut self, tool: AnnotationTool) {
        self.current_tool = tool;
    }

    pub fn set_color(&mut self, color: String) {
        self.current_color = color;
    }

    pub fn set_width(&mut self, width: f64) {
        self.current_width = width;
    }

    pub fn add_stroke(&mut self, stroke: AnnotationStroke) {
        if self.enabled {
            self.strokes.push(stroke);
        }
    }

    pub fn clear(&mut self) {
        self.strokes.clear();
    }

    pub fn undo(&mut self) {
        self.strokes.pop();
    }

    pub fn strokes(&self) -> &[AnnotationStroke] {
        &self.strokes
    }
}
