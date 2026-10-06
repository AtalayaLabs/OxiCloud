use chrono::{DateTime, NaiveDate, Utc};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AddressBook {
    id: Uuid,
    name: String,
    owner_id: String,
    description: Option<String>,
    color: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

/// Owned decomposition of an [`AddressBook`] (mirrors `FileParts`/`UserParts`).
/// Lets `AddressBookDto::from` MOVE `name`/`description`/`color`/`owner_id`
/// instead of cloning them on every CardDAV discovery listing
/// (benches/ROUND20.md §A4).
pub struct AddressBookParts {
    pub id: Uuid,
    pub name: String,
    pub owner_id: String,
    pub description: Option<String>,
    pub color: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl AddressBook {
    /// Decompose into [`AddressBookParts`], moving every owned field out
    /// (exhaustive destructure — compiler-checked against added fields).
    pub fn into_parts(self) -> AddressBookParts {
        let AddressBook {
            id,
            name,
            owner_id,
            description,
            color,
            created_at,
            updated_at,
        } = self;
        AddressBookParts {
            id,
            name,
            owner_id,
            description,
            color,
            created_at,
            updated_at,
        }
    }
}

impl AddressBook {
    /// Creates a new AddressBook with generated id and timestamps
    pub fn new(
        name: String,
        owner_id: String,
        description: Option<String>,
        color: Option<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            name,
            owner_id,
            description,
            color,
            created_at: now,
            updated_at: now,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_raw(
        id: Uuid,
        name: String,
        owner_id: String,
        description: Option<String>,
        color: Option<String>,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            name,
            owner_id,
            description,
            color,
            created_at,
            updated_at,
        }
    }

    // --- Getters ---
    pub fn id(&self) -> &Uuid {
        &self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn owner_id(&self) -> &str {
        &self.owner_id
    }
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
    pub fn color(&self) -> Option<&str> {
        self.color.as_deref()
    }
    pub fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }
    pub fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }

    // --- Setters for mutable operations ---
    pub fn set_name(&mut self, name: String) {
        self.name = name;
        self.updated_at = Utc::now();
    }
    pub fn set_description(&mut self, description: Option<String>) {
        self.description = description;
        self.updated_at = Utc::now();
    }
    pub fn set_color(&mut self, color: Option<String>) {
        self.color = color;
        self.updated_at = Utc::now();
    }
    pub fn set_updated_at(&mut self, updated_at: DateTime<Utc>) {
        self.updated_at = updated_at;
    }
}

impl Default for AddressBook {
    fn default() -> Self {
        Self::new(
            "Default Address Book".to_string(),
            "default".to_string(),
            None,
            None,
        )
    }
}

#[derive(Debug, Clone)]
pub struct Email {
    pub email: String,
    pub r#type: String, // home, work, other
    pub is_primary: bool,
}

#[derive(Debug, Clone)]
pub struct Phone {
    pub number: String,
    pub r#type: String, // mobile, home, work, fax, other
    pub is_primary: bool,
}

#[derive(Debug, Clone)]
pub struct Address {
    pub street: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
    pub r#type: String, // home, work, other
    pub is_primary: bool,
}

#[derive(Debug, Clone)]
pub struct Contact {
    id: Uuid,
    address_book_id: Uuid,
    uid: String,
    full_name: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
    nickname: Option<String>,
    email: Vec<Email>,
    phone: Vec<Phone>,
    address: Vec<Address>,
    organization: Option<String>,
    title: Option<String>,
    notes: Option<String>,
    photo_url: Option<String>,
    birthday: Option<NaiveDate>,
    anniversary: Option<NaiveDate>,
    vcard: String,
    etag: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl Contact {
    /// Creates a new Contact with generated id, uid, etag and timestamps
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        address_book_id: Uuid,
        full_name: Option<String>,
        first_name: Option<String>,
        last_name: Option<String>,
        nickname: Option<String>,
        email: Vec<Email>,
        phone: Vec<Phone>,
        address: Vec<Address>,
        organization: Option<String>,
        title: Option<String>,
        notes: Option<String>,
        photo_url: Option<String>,
        birthday: Option<NaiveDate>,
        anniversary: Option<NaiveDate>,
        vcard: String,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            address_book_id,
            uid: format!("{}@oxicloud", Uuid::new_v4()),
            full_name,
            first_name,
            last_name,
            nickname,
            email,
            phone,
            address,
            organization,
            title,
            notes,
            photo_url,
            birthday,
            anniversary,
            vcard,
            etag: Uuid::new_v4().to_string(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Reconstructs from persistence (no validation)
    #[allow(clippy::too_many_arguments)]
    pub fn from_raw(
        id: Uuid,
        address_book_id: Uuid,
        uid: String,
        full_name: Option<String>,
        first_name: Option<String>,
        last_name: Option<String>,
        nickname: Option<String>,
        email: Vec<Email>,
        phone: Vec<Phone>,
        address: Vec<Address>,
        organization: Option<String>,
        title: Option<String>,
        notes: Option<String>,
        photo_url: Option<String>,
        birthday: Option<NaiveDate>,
        anniversary: Option<NaiveDate>,
        vcard: String,
        etag: String,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            address_book_id,
            uid,
            full_name,
            first_name,
            last_name,
            nickname,
            email,
            phone,
            address,
            organization,
            title,
            notes,
            photo_url,
            birthday,
            anniversary,
            vcard,
            etag,
            created_at,
            updated_at,
        }
    }

    // --- Getters ---
    pub fn id(&self) -> &Uuid {
        &self.id
    }
    pub fn address_book_id(&self) -> &Uuid {
        &self.address_book_id
    }
    pub fn uid(&self) -> &str {
        &self.uid
    }
    pub fn full_name(&self) -> Option<&str> {
        self.full_name.as_deref()
    }
    pub fn first_name(&self) -> Option<&str> {
        self.first_name.as_deref()
    }
    pub fn last_name(&self) -> Option<&str> {
        self.last_name.as_deref()
    }
    pub fn nickname(&self) -> Option<&str> {
        self.nickname.as_deref()
    }
    pub fn email(&self) -> &[Email] {
        &self.email
    }
    pub fn phone(&self) -> &[Phone] {
        &self.phone
    }
    pub fn address(&self) -> &[Address] {
        &self.address
    }
    pub fn organization(&self) -> Option<&str> {
        self.organization.as_deref()
    }
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
    pub fn notes(&self) -> Option<&str> {
        self.notes.as_deref()
    }
    pub fn photo_url(&self) -> Option<&str> {
        self.photo_url.as_deref()
    }
    pub fn birthday(&self) -> Option<&NaiveDate> {
        self.birthday.as_ref()
    }
    pub fn anniversary(&self) -> Option<&NaiveDate> {
        self.anniversary.as_ref()
    }
    pub fn vcard(&self) -> &str {
        &self.vcard
    }
    pub fn etag(&self) -> &str {
        &self.etag
    }
    pub fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }
    pub fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }

    // --- Owned getters for persistence layer bind() calls ---
    pub fn full_name_owned(&self) -> Option<String> {
        self.full_name.clone()
    }
    pub fn first_name_owned(&self) -> Option<String> {
        self.first_name.clone()
    }
    pub fn last_name_owned(&self) -> Option<String> {
        self.last_name.clone()
    }
    pub fn nickname_owned(&self) -> Option<String> {
        self.nickname.clone()
    }
    pub fn organization_owned(&self) -> Option<String> {
        self.organization.clone()
    }
    pub fn title_owned(&self) -> Option<String> {
        self.title.clone()
    }
    pub fn notes_owned(&self) -> Option<String> {
        self.notes.clone()
    }
    pub fn photo_url_owned(&self) -> Option<String> {
        self.photo_url.clone()
    }

    // --- Setters for mutable operations (contact_service.rs needs these) ---
    pub fn set_full_name(&mut self, v: Option<String>) {
        self.full_name = v;
    }
    pub fn set_first_name(&mut self, v: Option<String>) {
        self.first_name = v;
    }
    pub fn set_last_name(&mut self, v: Option<String>) {
        self.last_name = v;
    }
    pub fn set_nickname(&mut self, v: Option<String>) {
        self.nickname = v;
    }
    pub fn set_organization(&mut self, v: Option<String>) {
        self.organization = v;
    }
    pub fn set_title(&mut self, v: Option<String>) {
        self.title = v;
    }
    pub fn set_notes(&mut self, v: Option<String>) {
        self.notes = v;
    }
    pub fn set_photo_url(&mut self, v: Option<String>) {
        self.photo_url = v;
    }
    pub fn set_birthday(&mut self, v: Option<NaiveDate>) {
        self.birthday = v;
    }
    pub fn set_anniversary(&mut self, v: Option<NaiveDate>) {
        self.anniversary = v;
    }
    pub fn set_vcard(&mut self, vcard: String) {
        self.vcard = vcard;
    }
    pub fn set_etag(&mut self, etag: String) {
        self.etag = etag;
    }
    pub fn set_updated_at(&mut self, updated_at: DateTime<Utc>) {
        self.updated_at = updated_at;
    }
    pub fn set_address_book_id(&mut self, id: Uuid) {
        self.address_book_id = id;
    }
    pub fn set_uid(&mut self, uid: String) {
        self.uid = uid;
    }

    // --- Collection mutators ---
    pub fn push_email(&mut self, e: Email) {
        self.email.push(e);
    }
    pub fn push_phone(&mut self, p: Phone) {
        self.phone.push(p);
    }
    pub fn push_address(&mut self, a: Address) {
        self.address.push(a);
    }
    pub fn set_email(&mut self, email: Vec<Email>) {
        self.email = email;
    }
    pub fn set_phone(&mut self, phone: Vec<Phone>) {
        self.phone = phone;
    }
    pub fn set_address(&mut self, address: Vec<Address>) {
        self.address = address;
    }
    pub fn email_is_empty(&self) -> bool {
        self.email.is_empty()
    }
    pub fn phone_is_empty(&self) -> bool {
        self.phone.is_empty()
    }
    pub fn address_is_empty(&self) -> bool {
        self.address.is_empty()
    }

    // --- Consuming methods for ownership transfer ---
    pub fn into_email(self) -> Vec<Email> {
        self.email
    }
    pub fn into_parts(self) -> ContactParts {
        ContactParts {
            id: self.id,
            address_book_id: self.address_book_id,
            uid: self.uid,
            full_name: self.full_name,
            first_name: self.first_name,
            last_name: self.last_name,
            nickname: self.nickname,
            email: self.email,
            phone: self.phone,
            address: self.address,
            organization: self.organization,
            title: self.title,
            notes: self.notes,
            photo_url: self.photo_url,
            birthday: self.birthday,
            anniversary: self.anniversary,
            vcard: self.vcard,
            etag: self.etag,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// Holds all Contact fields by value, for when ownership transfer is needed
pub struct ContactParts {
    pub id: Uuid,
    pub address_book_id: Uuid,
    pub uid: String,
    pub full_name: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub nickname: Option<String>,
    pub email: Vec<Email>,
    pub phone: Vec<Phone>,
    pub address: Vec<Address>,
    pub organization: Option<String>,
    pub title: Option<String>,
    pub notes: Option<String>,
    pub photo_url: Option<String>,
    pub birthday: Option<NaiveDate>,
    pub anniversary: Option<NaiveDate>,
    pub vcard: String,
    pub etag: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Default for Contact {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            address_book_id: Uuid::new_v4(),
            uid: format!("{}@oxicloud", Uuid::new_v4()),
            full_name: None,
            first_name: None,
            last_name: None,
            nickname: None,
            email: Vec::new(),
            phone: Vec::new(),
            address: Vec::new(),
            organization: None,
            title: None,
            notes: None,
            photo_url: None,
            birthday: None,
            anniversary: None,
            vcard: "BEGIN:VCARD\nVERSION:3.0\nEND:VCARD".to_string(),
            etag: Uuid::new_v4().to_string(),
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContactGroup {
    id: Uuid,
    address_book_id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl ContactGroup {
    /// Creates a new ContactGroup with generated id and timestamps
    pub fn new(address_book_id: Uuid, name: String) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            address_book_id,
            name,
            created_at: now,
            updated_at: now,
        }
    }

    /// Reconstructs from persistence
    pub fn from_raw(
        id: Uuid,
        address_book_id: Uuid,
        name: String,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            address_book_id,
            name,
            created_at,
            updated_at,
        }
    }

    // --- Getters ---
    pub fn id(&self) -> &Uuid {
        &self.id
    }
    pub fn address_book_id(&self) -> &Uuid {
        &self.address_book_id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }
    pub fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }

    // --- Setters ---
    pub fn set_name(&mut self, name: String) {
        self.name = name;
        self.updated_at = Utc::now();
    }
    pub fn set_updated_at(&mut self, updated_at: DateTime<Utc>) {
        self.updated_at = updated_at;
    }
}

impl Default for ContactGroup {
    fn default() -> Self {
        ContactGroup::new(Uuid::new_v4(), "New Group".to_string())
    }
}
