-- Drop `is_public` from carddav.address_books.
--
-- The last of the three. This one was live in the same way the playlist
-- flag was, and slightly worse placed: `ContactService` had a dedicated
-- gate, `require_address_book_read_or_public`, used by thirteen read
-- paths, which returned the book before consulting the engine at all if
-- the flag was set. One boolean therefore granted every authenticated
-- user Read on the book and — because the gate also fronted contacts,
-- groups and group memberships — on everything inside it.
--
-- `list_address_books` additionally unioned every flagged book into each
-- caller's listing, so a public book appeared in strangers' CardDAV
-- discovery.
--
-- Removed for the same reason as the playlist flag: "every account on
-- this instance" is not a sharing scope, no anonymous client could ever
-- use it (CardDAV is behind auth), and a second authorization surface
-- beside `storage.role_grants` is one more place for a read gate to be
-- wrong. Sharing an address book with everyone belongs in the grant
-- table, where expiry, audit and revocation apply.
--
-- Affected books keep their owner and their existing grants; nobody else
-- sees them. No contact, group or membership is deleted.
DO $$
DECLARE
    public_rows BIGINT;
BEGIN
    SELECT count(*) INTO public_rows
    FROM carddav.address_books
    WHERE is_public;

    IF public_rows > 0 THEN
        RAISE WARNING
            'dropping carddav.address_books.is_public: % address book(s) were flagged public and are now reachable only through their grants (owner + existing shares). No contacts were removed.',
            public_rows;
    END IF;
END
$$;

ALTER TABLE carddav.address_books DROP COLUMN IF EXISTS is_public;
