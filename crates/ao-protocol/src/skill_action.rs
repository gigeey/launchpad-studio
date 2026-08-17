#[derive(Debug, Clone)]
pub struct WriteSkillAction {
    pub title: String,
    pub description: String,
    pub content: String,
    pub focus_path: Option<String>,
    pub override_existing: bool,
}
