# Administrators & the owner

Most people who use OxiCloud never see this page. It matters if you
help run the server.

There are two levels of responsibility, and the difference only shows
up when one administrator tries to change another.

| Role | What it means |
|---|---|
| **User** | A normal account. Has their own space, shares what they choose. |
| **Administrator** | Can manage users — create accounts, set quotas, reset passwords, deactivate people. |
| **Owner** | The person responsible for this server. Exactly one account, marked with a crown. |

## Administrators cannot change each other

An administrator can manage **users**, but not other administrators.
They cannot change a colleague's role, reset their password,
deactivate them, or delete their account.

This is deliberate. Resetting someone's password means being able to
sign in as them, so without this rule any administrator could take
over any other administrator's account — and lock the rest of the
team out, whether by mistake or on purpose.

The owner can manage administrators. Nobody can manage the owner.

## Your own account

You can always change your own password and profile from your account
settings, and you can set your own storage quota.

What you cannot do to yourself: change your own role, deactivate your
own account, or delete it. These would either lock you out or leave
the server short an administrator, and undoing them needs someone
else.

## Who the owner is

The owner's row in the user list carries a crown. On a new server
it's whoever completed the initial setup.

The owner is the one account no administrator can touch, which makes
it the account to keep recoverable: a working email address on it, and
someone who still has access to that mailbox.

## Handing the server over

If you're the owner and someone else is taking over — you're leaving,
or handing off the role — open the user list, find them, and choose
**Transfer ownership**.

They must be a regular account on this server, active, and not an
account that signs in through an external identity provider.

Two things happen at once: they become the owner, and you become an
ordinary administrator. You cannot undo it yourself afterwards — only
the new owner can transfer it back. So do this with them, not for
them.

## If there's no owner

A server upgraded from an older release starts without one, because
OxiCloud won't guess which administrator should hold it.

Everything keeps working except administrator-on-administrator
changes, which are refused until someone is named. Whoever runs the
server can set this up; ask them to follow the upgrade notes in the
installation guide.
