use crate::domain::entities::contact::AddressBook;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressBookDto {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub description: Option<String>,
    pub color: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Default for AddressBookDto {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: "Default Address Book".to_string(),
            owner_id: "default".to_string(),
            description: None,
            color: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
}

impl From<AddressBook> for AddressBookDto {
    fn from(book: AddressBook) -> Self {
        // Owned entity → move the owned fields instead of cloning through the
        // borrowing accessors (benches/ROUND20.md §A4).
        let p = book.into_parts();
        Self {
            id: p.id.to_string(),
            name: p.name,
            owner_id: p.owner_id,
            description: p.description,
            color: p.color,
            created_at: p.created_at,
            updated_at: p.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAddressBookDto {
    pub name: String,
    pub owner_id: String,
    pub description: Option<String>,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateAddressBookDto {
    pub name: Option<String>,
    pub description: Option<String>,
    pub color: Option<String>,
    pub user_id: String, // Current user making the update
}
