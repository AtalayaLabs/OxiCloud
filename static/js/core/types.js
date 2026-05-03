/**
 * @typedef {Object} FolderInfo
 * @property {string} category
 * @property {number} created_at - timestamp
 * @property {string} icon_class
 * @property {string} icon_special_class
 * @property {string} id the uniq id of the folder
 * @property {boolean} is_root
 * @property {number} modified_at
 * @property {string} name
 * @property {string} owner_id
 * @property {string|null} parent_id the folder parent (null if is_root)
 * @property {string} path the full path
 */

/**
 * @typedef {Object} FileInfo
 * @property {string} category
 * @property {number} created_at - timestamp
 * @property {string} icon_class
 * @property {string} icon_special_class
 * @property {string} id the uniq id of the folder
 * @property {string} mime_type
 * @property {number} modified_at - timestamp
 * @property {string} name
 * @property {string} owner_id
 * @property {string} folder_id the folder parent
 * @property {string} path the full path
 * @property {number} size
 * @property {string} size_formatted
 * @property {number} sort_date
 */

/**
 * @typedef {Object} SharePermissions
 * @property {boolean} read
 * @property {boolean} reshare
 * @property {boolean} write
 */

/**
 * @typedef {Object} Share
 * @property {number} access_count
 * @property {number} created_at - timestamp
 * @property {String} created_by
 * @property {number} expires_at - timestamp
 * @property {boolean} has_password
 * @property {string} id
 * @property {string} item_id
 * @property {string} item_name
 * @property {string} item_type
 * @property {SharePermissions} permissions
 * @property {string | null} token
 * @property {string} url
 */
