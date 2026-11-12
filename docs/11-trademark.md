# 11 — Naming and trademarks

Status: revision 1 · Last revised: 2026-11-06 (decision D15)

Mastodon is a registered trademark of Mastodon GmbH, and the licence on their
software does not grant a licence to their marks. This project therefore keeps
the mark out of its identity and only uses it to say what it works with.

Source: <https://joinmastodon.org/trademark> (quoted below).

## The rule we follow

From the policy's general guidelines:

> - "Do not use or register, in whole or in part, the Mastodon marks as part of
>   your own or any other trade mark, service mark, company name, trade name,
>   product name, or service name."
> - "You may use the Mastodon word mark in referential phrases such as 'for',
>   'for use with', or 'compatible with'."

And for projects built on the open-source code:

> - "You must choose your own branding, logos, and trade marks that denote your
>   unique identity so as to clearly signal to users that there is no affiliation
>   with or endorsement by Mastodon GmbH."
> - "You may use word marks, but not our logos, in truthful statements that
>   describe the relationship between your software and ours."

## What this means in practice

| Context | Allowed | Not allowed |
| --- | --- | --- |
| Product, repository, crate, binary, service name | `nodus`, `nodus-client-sdk`, `nodus-feeds` | `mastodon-*`, `Nodus for Mastodon` as a *name*, any spelling variant (`mstdn`, `mastodoon`) |
| Domain name | `nodus.example` | `mastodon-nodus.com`, `nodus-mastodon.com` (mark in any level of the name) |
| Prose, docs, marketing | "A recommendation engine for use with Mastodon", "reads the Mastodon API", "compatible with Mastodon 4.2+" | "the Nodus Mastodon client", "Mastodon's official recommender" |
| Logos and artwork | — | The Mastodon logo, the elephant mark, or anything confusingly similar |
| Instance data | Displaying user content published using Mastodon software | Presenting instance content as if Mastodon GmbH produced it |
| Affiliation claims | "Not affiliated with or endorsed by Mastodon GmbH" | Anything implying review, approval, partnership or sponsorship |

The disclaimer goes in every project README and in the client's about screen:

> Nodus is not affiliated with, endorsed by, or sponsored by Mastodon GmbH.
> "Mastodon" is a trademark of Mastodon GmbH and is used here only to describe
> compatibility.

## Checklist for anything new

Before naming a crate, binary, repository, endpoint, feature or folder:

1. Does the name contain `mastodon` (or a variant) as a *name*? Rename it — keep
   the word only in the sentence that explains compatibility.
2. Does English prose without the mark still explain what it does? If not, the
   name is doing brand work it should not do.
3. If the mark appears in a screenshot, fixture or example, is it a truthful
   statement about the software ("timeline from a Mastodon instance")? If yes,
   fine; if it reads as endorsement, remove it.
4. Are we using any Mastodon artwork? Do not; use our own or none.

Existing code is already compliant: the crate prefix is `nodus-`, and the module
that implements the instance API is `nodus-client-core`, not `mastodon`.

## Precedent and risk

Third-party clients (Elk, Phanpy, Tusky, Mona) follow the same pattern: their own
name, with compatibility stated in prose. That is the safe shape, and it also
avoids the second problem with carrying the mark — a name that reads as
"the Mastodon client" invites users to hold us responsible for instance content
and for Mastodon GmbH's product decisions.

The policy can be revoked or changed; re-read it before a public launch and before
registering a domain. Contact for permission: `trademark@joinmastodon.org`
(requests not approved within ten business days count as denied).
