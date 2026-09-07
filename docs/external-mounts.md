# External mounts

External mounts expose files from a provider such as a host directory inside an
OxiCloud drive. Enable the feature with `OXICLOUD_ENABLE_EXTERNAL_MOUNTS=true`,
then configure mounts from **Administration > External Mounts**.

Each mount must be attached to a drive. The mount-root is a normal folder in
that drive, and access to all provider content is inherited from the drive's
membership and roles:

- a mount in a personal drive is visible only to that drive's owner;
- a mount in a shared drive is visible to members who can read the drive;
- mutations additionally require the corresponding drive permission and are
  always rejected when the mount is configured as read-only.

Removing a mount deletes only its OxiCloud mount-root and configuration. It does
not delete the provider's root directory. Deleting files or folders *inside* a
writable mount is permanent because external mounts do not use OxiCloud trash.

Mounts created before drive selection was introduced remain attached to their
existing drive, normally the configuring administrator's personal drive. To
make one available through a shared drive, remove it and recreate it with that
shared drive selected; removing the old mount does not remove host content.
