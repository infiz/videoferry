use std::collections::VecDeque;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;

use crate::QueueSettings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueStatus {
    Pending,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueueTask {
    pub id: String,
    pub name: String,
    pub targets: Vec<PathBuf>,
    pub source_root: Option<PathBuf>,
    pub queued_time: String,
    pub complete_time: String,
    pub settings: QueueSettings,
    pub status: QueueStatus,
    pub error: Option<String>,
    pub skipped_paths: Vec<PathBuf>,
}

impl QueueTask {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        targets: Vec<PathBuf>,
        settings: QueueSettings,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            targets,
            source_root: None,
            queued_time: String::new(),
            complete_time: String::new(),
            settings,
            status: QueueStatus::Pending,
            error: None,
            skipped_paths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueError {
    DuplicateId(String),
    UnknownId(String),
    InvalidMove,
}

impl Display for QueueError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateId(id) => write!(formatter, "queue task id already exists: {id}"),
            Self::UnknownId(id) => write!(formatter, "queue task id not found: {id}"),
            Self::InvalidMove => formatter.write_str("queue task cannot be moved there"),
        }
    }
}

impl Error for QueueError {}

#[derive(Debug, Default)]
pub struct Queue {
    tasks: VecDeque<QueueTask>,
}

impl Queue {
    #[must_use]
    pub fn tasks(&self) -> &VecDeque<QueueTask> {
        &self.tasks
    }

    /// Adds a task at the end of the queue.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::DuplicateId`] when the identifier is already used.
    pub fn add(&mut self, task: QueueTask) -> Result<(), QueueError> {
        if self.tasks.iter().any(|existing| existing.id == task.id) {
            return Err(QueueError::DuplicateId(task.id));
        }
        self.tasks.push_back(task);
        Ok(())
    }

    /// Removes and returns a task.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::UnknownId`] when no task has the supplied identifier.
    pub fn remove(&mut self, id: &str) -> Result<QueueTask, QueueError> {
        let index = self
            .tasks
            .iter()
            .position(|task| task.id == id)
            .ok_or_else(|| QueueError::UnknownId(id.to_owned()))?;
        self.tasks
            .remove(index)
            .ok_or_else(|| QueueError::UnknownId(id.to_owned()))
    }

    pub fn clear(&mut self) {
        self.tasks.clear();
    }

    /// Moves a task by a signed number of queue positions.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::UnknownId`] for an unknown task or
    /// [`QueueError::InvalidMove`] when the destination is outside the queue.
    pub fn move_by(&mut self, id: &str, delta: isize) -> Result<(), QueueError> {
        let from = self
            .tasks
            .iter()
            .position(|task| task.id == id)
            .ok_or_else(|| QueueError::UnknownId(id.to_owned()))?;
        let to = from
            .checked_add_signed(delta)
            .ok_or(QueueError::InvalidMove)?;
        if to >= self.tasks.len() {
            return Err(QueueError::InvalidMove);
        }
        let task = self.tasks.remove(from).ok_or(QueueError::InvalidMove)?;
        self.tasks.insert(to, task);
        Ok(())
    }

    /// Moves a task to an absolute queue position.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::UnknownId`] for an unknown task or
    /// [`QueueError::InvalidMove`] when the destination is outside the queue.
    pub fn move_to(&mut self, id: &str, to: usize) -> Result<(), QueueError> {
        let from = self
            .tasks
            .iter()
            .position(|task| task.id == id)
            .ok_or_else(|| QueueError::UnknownId(id.to_owned()))?;
        if to >= self.tasks.len() {
            return Err(QueueError::InvalidMove);
        }
        if from == to {
            return Ok(());
        }
        let task = self.tasks.remove(from).ok_or(QueueError::InvalidMove)?;
        self.tasks.insert(to, task);
        Ok(())
    }

    #[must_use]
    pub fn next_pending_id(&self) -> Option<&str> {
        self.tasks
            .iter()
            .find(|task| task.status == QueueStatus::Pending)
            .map(|task| task.id.as_str())
    }

    /// Updates the lifecycle status of a queued task.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::UnknownId`] when no task has the supplied identifier.
    pub fn set_status(&mut self, id: &str, status: QueueStatus) -> Result<(), QueueError> {
        let task = self
            .tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or_else(|| QueueError::UnknownId(id.to_owned()))?;
        task.status = status;
        Ok(())
    }

    /// Stores or clears a task's last error.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::UnknownId`] when no task has the supplied identifier.
    pub fn set_error(&mut self, id: &str, error: Option<String>) -> Result<(), QueueError> {
        let task = self
            .tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or_else(|| QueueError::UnknownId(id.to_owned()))?;
        task.error = error;
        Ok(())
    }

    #[must_use]
    pub fn task(&self, id: &str) -> Option<&QueueTask> {
        self.tasks.iter().find(|task| task.id == id)
    }

    #[must_use]
    pub fn task_mut(&mut self, id: &str) -> Option<&mut QueueTask> {
        self.tasks.iter_mut().find(|task| task.id == id)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Queue, QueueError, QueueTask};
    use crate::QueueSettings;

    fn task(id: &str) -> QueueTask {
        QueueTask::new(
            id,
            format!("Task {id}"),
            vec![PathBuf::from(format!("{id}.mkv"))],
            QueueSettings::default(),
        )
    }

    #[test]
    fn queue_reorders_without_losing_tasks() {
        let mut queue = Queue::default();
        queue.add(task("one")).unwrap();
        queue.add(task("two")).unwrap();
        queue.add(task("three")).unwrap();
        queue.move_by("two", -1).unwrap();
        let ids: Vec<_> = queue.tasks().iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["two", "one", "three"]);

        queue.move_to("three", 0).unwrap();
        let ids: Vec<_> = queue.tasks().iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["three", "two", "one"]);
    }

    #[test]
    fn queue_rejects_duplicate_ids() {
        let mut queue = Queue::default();
        queue.add(task("one")).unwrap();
        assert_eq!(
            queue.add(task("one")),
            Err(QueueError::DuplicateId("one".to_owned()))
        );
    }

    #[test]
    fn queued_task_settings_can_be_reviewed_in_place() {
        let mut queue = Queue::default();
        queue.add(task("review")).unwrap();
        queue
            .task_mut("review")
            .unwrap()
            .settings
            .slideshow_image_paths
            .push(PathBuf::from("photo.jpg"));
        assert_eq!(
            queue.task("review").unwrap().settings.slideshow_image_paths,
            [PathBuf::from("photo.jpg")]
        );
    }

    #[test]
    fn queue_can_be_cleared_after_processing() {
        let mut queue = Queue::default();
        queue.add(task("one")).unwrap();
        queue.add(task("two")).unwrap();

        queue.clear();

        assert!(queue.tasks().is_empty());
        assert!(queue.next_pending_id().is_none());
    }
}
