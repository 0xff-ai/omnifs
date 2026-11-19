# Provider object SDK design

Status: implemented (the SDK surface and all nine providers run on this design)

## Purpose

Provider code should read as a typed filesystem tree. The provider-facing SDK is
object porcelain; WIT path dispatch is SDK plumbing. Provider authors describe
the path hierarchy and the domain behavior in `start()`, and the SDK lowers that
into lookup, list, read, cache, invalidation, stream, tree, and WIT operations.

This is the shape to preserve:

- object semantics are the default for provider concepts with identity and
  replayable canonical bytes;
- the same route API also describes structural directories, direct files,
  streams, and tree handoffs without pretending they are cached canonicals;
- file and dir are the filesystem nouns. Stream, tree, canonical,
  representation, computed, and direct are behaviors under those nouns;
- route topology remains visible in `start()`. Object implementations provide
  behavior only and do not register hidden paths;
- relative route faces compose the tree. They let an object directory list child
  objects, add aliases, or share an object declaration without nested route
  DSLs;
- collections return object entries, not anonymous paths. When a list payload is
  complete enough to satisfy the child object's canonical contract, the SDK
  stores that child canonical at listing time;
- collections carry typed cursors. The host stores cursor bytes opaquely, and
  providers receive typed cursors back on the next list call;
- canonical bytes can be raw upstream bytes or deterministic logical-object
  bytes in any format. JSON is just one format;
- provider-local caches, compatibility bridges, and repeated computed-file
  helpers disappear unless they own a real domain rule;
- raw path handlers remain the low-level escape hatch for routes that are not
  honestly anchored in object identity.

The end state is one provider route API. Object faces use canonical storage when
that is honest; direct, stream, tree, and structural faces declare their own
read contract.

## Design constraints

These constraints come from the design discussion and should survive API naming
changes:

- the object-shaped SDK is the default provider authoring surface. Raw path
  dispatch is plumbing and an escape hatch, not a peer SDK flavor;
- provider authors must be able to read the projected path hierarchy from
  `start()`. Object implementations own behavior, not hidden route topology;
- relative paths inside object blocks are real route declarations. Keep them;
- do not add syntactic nesting for deeper hierarchy. One top-level object block
  declares the faces under that object anchor; child objects get their own route
  face or object block;
- object anchors may be directory-shaped or file-shaped. A cacheable single file
  with its own identity, validator, and canonical bytes is still an object, and
  the route API must allow that shape at the top level as well as under a parent
  object;
- large host-resident artifacts with validators are not automatically objects.
  Use object shape when the provider needs identity, decode, computed faces, or
  child relationships; use blob shape when the important property is that bytes
  stay host-side;
- an object owns identity, load, decode, and canonical bytes. Identity is a named
  `#[path_captures]` key struct; tuple keys are unsupported;
- canonical bytes are replayable object bytes. Prefer the raw upstream response
  when that response is the object contract. Allow deterministic slicing,
  normalization, or assembly from a batch response, envelope, noisy server
  object, or multi-call response when the stored bytes are exactly what
  `decode` consumes and validator provenance is explicit;
- decoding belongs to the object. A private transport DTO is fine inside
  `decode`, but the public object API should not expose `IssueJson`-style
  helper types;
- formats are selected with normal type parameters or associated types, not
  const generics, attribute strings, or path-name type tricks;
- collections return child objects. When a list response contains complete
  child canonical bytes, the SDK stores them at listing time;
- collections carry typed cursors and page validators. The host stores cursor
  and validator bytes opaquely and feeds them back to the provider: the cursor
  to continue a listing, the validator to ask again for a page past its
  deadline;
- lazy and eager describe materialization from bytes already in hand. They never
  authorize an extra upstream fetch just to populate a computed field;
- providers describe file and directory characteristics: size, content type,
  stability, read mode, byte source, version or validator evidence. Providers do
  not declare TTLs, retention windows, or host cache policy;
- direct means the provider/upstream is invoked for the read. Do not use direct
  for bytes the host should dedupe, serve by handle, or feed to another host-side
  subsystem. A direct read may still project small sibling files from bytes it
  already fetched;
- blob means host-resident bytes, normally from a blob fetch, where the body
  should not cross the provider/WIT boundary. A blob can still carry exact size,
  content type, stability, and ETag/version evidence;
- preserve the blob toolchain as a host-owned capability: `fetch-blob` stores a
  body host-side and returns a runtime-local handle for file projection;
- `BlobId` is runtime-local. The host derives durable blob request and body
  identities from validated request facts; providers never name cache entries;
- stream and tree stay behaviors under file and directory faces;
- stability, mutability, and authority are separate axes. The current object SDK
  stays read-only until write semantics exist.

## Target route shape

The exact method names can move, but the call site should stay this plain.

```rust
#[omnifs_sdk::provider]
impl Provider for GitHub {
    fn start(r: &mut Router) -> Result<()> {
        r.object::<Owner>("/{owner}", |o| {
            o.dynamic();
            o.file("owner.json").canonical::<Json>()?;
            o.file("profile.md").representation::<Markdown>()?;
            o.dir("{repo}").collection::<Repo>(Owner::repos)?;
            Ok(())
        })?;

        r.object::<Repo>("/{owner}/{repo}", |o| {
            o.dynamic();
            o.file("repo.json").canonical::<Json>()?;
            o.dir("repo").tree(Repo::tree)?;
            o.dir("issues").choices(StateFilter::choices())?;
            o.dir("issues/{filter}").fed()?;
            o.dir("pulls").choices(StateFilter::choices())?;
            o.dir("pulls/{filter}").fed()?;
            o.dir("actions/runs").collection::<WorkflowRun>(Repo::workflow_runs)?;
            o.feed(Repo::feed).starts(["issues/{filter}", "pulls/{filter}"])?;
            Ok(())
        })?;

        r.object::<Issue>("/{owner}/{repo}/issues/{filter}/{number}", |o| {
            o.dynamic();
            o.fed();
            o.file("item.json").canonical::<Json>()?;
            o.file("item.md").representation::<Markdown>()?;
            o.file("title").fed()?;
            o.file("body").fed()?;
            o.file("state").fed()?;
            o.file("user").fed()?;
            o.dir("comments").fed()?;
            o.feed(Issue::feed).starts(["", "title", "body", "state", "user", "comments"])?;
            Ok(())
        })?;

        r.object::<PullRequest>("/{owner}/{repo}/pulls/{filter}/{number}", |o| {
            o.dynamic();
            o.fed();
            o.file("item.json").canonical::<Json>()?;
            o.file("item.md").representation::<Markdown>()?;
            o.file("title").fed()?;
            o.file("body").fed()?;
            o.file("state").fed()?;
            o.file("user").fed()?;
            o.file("diff")
                .blob(PullRequest::diff)
                .content_type("text/x-diff")?;
            o.dir("comments").fed()?;
            o.feed(PullRequest::feed)
                .starts(["", "title", "body", "state", "user", "comments"])?;
            Ok(())
        })?;

        r.object::<Comment>(
            "/{owner}/{repo}/{item_kind}/{filter}/{number}/comments/{comment_id}",
            |o| {
                o.dynamic();
                o.fed();
                o.file("comment.json").canonical::<Json>()?;
                o.file("comment.md").representation::<Markdown>()?;
                o.file("body.md").fed()?;
                o.file("author").fed()?;
                Ok(())
            },
        )?;

        r.object::<WorkflowRun>("/{owner}/{repo}/actions/runs/{run_id}", |o| {
            o.dynamic();
            o.file("run.json").canonical::<Json>()?;
            o.file("status.txt").computed(WorkflowRun::status)?;
            o.file("conclusion.txt").computed(WorkflowRun::conclusion)?;
            o.file("log")
                .direct(WorkflowRun::log)
                .content_type("text/plain")?;
            Ok(())
        })?;

        Ok(())
    }
}
```

What matters:

- `r.object::<Issue>(...)` does not repeat `IssueKey`. The object owns its key
  type.
- relative paths inside an object block are still route declarations. They are
  visible in `start()` and do not move topology into `Object::load`.
- direct, blob, stream, tree, and collection faces stay in the same route block
  when they are keyed by the same object route context. They do not become
  canonical object bytes.
- blob-backed file faces, such as PR diffs, keep large or host-cacheable bytes in
  the host blob store. They are not direct reads and do not require the provider
  to parse those bytes as an object.
- no nested route closures are needed. One object block declares the faces under
  that object anchor; child objects get their own top-level object block.

## Object traits

Keep the trait surface small. Do not add a hierarchy until real providers force
it.

```rust
trait Object: Sized {
    type Key: Key;
    type State;
    type Canonical: Format;

    async fn load(
        cx: &Cx<Self::State>,
        key: &Self::Key,
        since: Option<Validator>,
    ) -> Result<Load<Self>>;

    fn decode(bytes: &[u8]) -> Result<Self>;
}

impl Object for Repo {
    type Key = RepoKey;
    type State = State;
    type Canonical = Json;

    fn decode(bytes: &[u8]) -> Result<Self> {
        // A private transport DTO may exist here, but it is not part of the
        // public object contract.
        let dto = decode_repo_response(bytes)?;
        Ok(Self::from(dto))
    }
}

impl Object for RepoAlias {
    type Key = RepoAliasKey;
    type State = ();
    type Canonical = Json;

    fn decode(bytes: &[u8]) -> Result<Self> {
        Repo::decode(bytes).map(Self::from)
    }
}
```

Keys are named `#[path_captures]` structs; `#[path_captures]` emits the `impl
Key`. Providers with no state use `()`. Tuple keys (`type Key = (OwnerName,
RepoName)`) are unsupported: they would need SDK blanket `Key` impls parsing
positionally, and named structs keep capture meaning explicit.

The format type identifies the byte format. The object decides what those bytes
mean. `Object::load` returns a `Load` carrying the object value, canonical
bytes, validator evidence, and optional preloads. Cached reads decode the stored
canonical bytes. Do not require objects to serialize their Rust value as the
canonical, and do not re-fetch upstream just to rebuild cached faces. Any
canonical synthesis must be deterministic from the upstream payloads already in
hand.

Do not use const generics for names, paths, content types, or formats. They make
the call site worse and Rust does not support arbitrary string const generics as
a stable general tool. Use normal path arguments and type parameters.

## Route dispatch and warm rendering

An object route registers a typed leaf table. Dispatch must be able to resolve
an incoming request into:

```rust
object = Issue
key = IssueKey { owner, repo, filter, number }
target = Computed("body.md")
```

or:

```rust
object = Issue
key = IssueKey { owner, repo, filter, number }
target = Representation(Markdown)
```

If the host pushes cached canonical bytes for the matching object id, the SDK
serves without upstream:

- a canonical face returns the cached canonical bytes or a host-owned canonical
  byte source;
- a representation face renders from the cached canonical bytes;
- a computed face parses the cached canonical bytes and runs the registered field
  function;
- an object-backed file or directory face resolves through the child object's
  own canonical cache, load, and decode contract;
- a direct, stream, blob, or tree face uses its registered behavior, because it
  is not necessarily derivable from the canonical object.

Lazy computed leaves remain routable. Lazy only means "do not preload this leaf
while listing"; it does not mean "reload upstream on read." A lazy leaf read can
still render from cached canonical bytes.

## Filesystem faces

An object can expose one or more filesystem faces:

```rust
o.file("x.json").canonical::<Json>()           // cacheable object bytes
o.file("x.md").representation::<Markdown>()    // whole-object render
o.file("x.txt").computed(Object::field)          // computed file from object data
o.file("x.txt").lazy().computed(Object::field)   // visible but not preloaded
o.file("x.patch").object::<PatchObject>()      // child object as a file
o.file("x.bin").direct(Object::read)           // provider/upstream on read
o.file("x.tar").blob(Object::asset)            // host-resident bytes
o.file("x.log").stream(Object::open)           // ranged or live bytes
o.dir("repo").tree(Object::checkout)           // tree handoff
```

The same model must support file-shaped object anchors directly:

```rust
r.file_object::<DailyCollection>("/{day}/{collection}", |o| {
    o.dynamic();
    o.canonical::<Json>()?;
    Ok(())
})?;
```

A file-shaped object is still an object: it has identity, canonical bytes,
decode, validators, and preloads. It just projects as a file instead of a
directory with child leaves.

The path is the name. Do not make authors pass a second canonical name when the
file path already names the bytes.

The main face categories are:

- **Canonical.** The object's replayable stored bytes in their declared format.
- **Representation.** A whole-object render from canonical bytes, selected by
  format/content type.
- **Computed.** A named file projected from the parsed object and route key,
  selected by leaf path.
- **Object face.** A file or directory backed by a child object with its own
  identity, load, decode, canonical bytes, and validator.
- **Direct.** Bytes loaded by invoking the provider/upstream for the read. Use
  this for query-shaped, transformed, or uncached reads. Do not use it when the
  host should own the raw body. Direct reads may fan out to several callouts and
  fold partial failures when that is the file's behavior.
- **Blob.** Host-resident bytes, normally fetched into the host blob store, whose
  body should not cross the provider boundary. Blob faces can still have exact
  size, content type, stability, and ETag/version evidence.
- **Stream.** A ranged read face. `Live` belongs here, not on the object as a
  whole.
- **Tree.** A host-materialized subtree handoff.
- **Directory.** A collection of child object entries.

## Attributes, stability, and mutability

Every file face has attributes. The object declaration supplies defaults, and a
file face may override when its byte source differs.

```rust
r.object::<PaperVersion>("/{paper}/{version}", |o| {
    o.stability(|key| {
        if key.version.is_numbered() {
            Stability::Stable
        } else {
            Stability::Dynamic
        }
    });

    o.file("paper.atom").canonical::<Atom>()?;
    o.file("paper.json").representation::<Json>()?;
    o.file("paper.pdf")
        .blob(PaperVersion::pdf)
        .attrs(PaperVersion::pdf_attrs)?;
    Ok(())
})?;
```

The attribute contract includes:

- `FileSize::Exact(n)`, `FileSize::NonZero`, or `FileSize::Unknown`;
- `Stability::Stable` for immutable identities;
- `Stability::Dynamic` for snapshot reads that may change between reads;
- `Stability::Live` only for ranged stream faces that may change while being
  observed;
- optional `VersionToken` validators such as ETags, resource versions,
  timestamps, or commit SHAs;
- content type, inferred from the path when obvious and overridden when not.

`Stable`, `Dynamic`, and `Live` describe byte behavior for a projected file or
object face. They are not provider-authored TTLs. The host owns cache retention,
eviction, revalidation strategy, and whether cached bytes are used for a
particular operation.

Mutability is a separate filesystem policy. A dynamic Docker container,
Kubernetes object, or Linear issue can still be read-only from the mount. The
object SDK should stay implicitly read-only until the first write path exists.

Do not overload `Dynamic` to mean writable. When writes land, they should be
explicit, atomic, and auditable, matching the broader project direction for
draft/transaction-style mutations.

Provider authority is also separate from mutability. Auth schemes, HTTP
domains, git access, preopens, local sockets, and events belong in manifests and
provider declarations. Route declarations consume that authority; they do not
grant it silently.

## Content type

All renders, including fields, should produce a content type. The distinction is
what the content type identifies.

Representations are selected by format/content type:

```rust
o.file("item.md").representation::<Markdown>()?;
o.file("item.json").canonical::<Json>()?;
```

Computed fields are selected by path and carry content type as metadata:

```rust
o.file("title.txt").computed(Issue::title)?;
o.file("state.txt").computed(Issue::state)?;
o.file("body.md").lazy().computed(Issue::body_markdown)?;
```

`title.txt`, `state.txt`, and `author.txt` can all be `text/plain`, so content
type cannot identify the field. The path identifies the field; content type
describes the bytes.

## Eager and lazy computed leaves

`computed` is eager by default:

```rust
o.file("title.txt").computed(Issue::title)?;
```

Eager means: when the object value is already in hand while listing an object
anchor or a collection entry, the SDK preloads that computed file into the view
cache. Eager does not mean: fetch upstream solely to populate a field.

Use `lazy()` when the leaf is large, expensive, rarely read, or would blow the
preload budget:

```rust
o.file("body.md").lazy().computed(Issue::body_markdown)?;
```

Lazy leaves are still visible in directory listings and still route through the
object leaf table. When read, they can render from host-pushed cached canonical
bytes. If no matching canonical is cached, the SDK calls `Object::load` and then
computes the requested leaf.

Eager computed leaves must produce inline bytes and must respect eager response
budgets. Large bytes should be represented as lazy computed leaves, object-backed
file faces, blob faces, or ranged stream faces.

## Byte sources

The object API must preserve the byte-source distinctions that affect standard
filesystem behavior:

- inline bytes for small eager preloads;
- body bytes for a materialized read response;
- deferred full reads for file entries whose content loads later;
- direct provider reads for bytes with no replayable object contract;
- ranged reads for large or streaming content;
- blobs for host-resident bytes that should not cross back through the provider,
  including bytes later served as a file or extracted into a `TreeRef`;
- canonical bytes for replayable object storage.

`Live` requires a ranged stream face:

```rust
o.file("logs/{container}.log")
    .stream(Pod::logs)
    .size(FileSize::Unknown)
    .live()?;
```

A whole object should normally be `Stable` or `Dynamic`; `Live` is a property of
one file face, not a property of every representation and computed leaf under the
object.

## Collections

Collections list child objects. They also describe what, if anything, the list
payload can safely preload. Pagination and completeness are part of this
contract, not separate provider conventions.

The route declaration stays plain. The list function signature selects the
cursor type:

```rust
impl Owner {
    async fn repos(self, cx: ListCx<GitHubCursor>) -> Result<Collection<Repo, GitHubCursor>> {
        let page = github::list_repos(&cx, self.login(), cx.cursor()).await?;

        Ok(Collection::page(page.items.into_iter().map(|row| {
            let key = RepoKey {
                owner: self.login().to_owned(),
                repo: row.name.clone(),
            };

            CollectionEntry::fresh(key, Repo::from(row.object), row.canonical)
        }))
        .next(page.next_cursor))
    }
}
```

Use the same shape for arXiv, even though the upstream cursor is different:

```rust
impl CategoryPapers {
    async fn list(self, cx: ListCx<ArxivCursor>) -> Result<Collection<Paper, ArxivCursor>> {
        let page = arxiv::category_page(&self, cx.cursor()).await?;

        Ok(Collection::page(page.entries.into_iter().map(|entry| {
            CollectionEntry::fresh(entry.key(), Paper::from(entry.atom), entry.atom_bytes)
        }))
        .next(page.next_cursor))
    }
}
```

The minimal SDK surface is:

```rust
enum Collection<T: Object, C = NoCursor> {
    Complete {
        entries: Vec<CollectionEntry<T>>,
        validator: Option<PageValidator>,
    },
    Page {
        entries: Vec<CollectionEntry<T>>,
        next: C,
        validator: Option<PageValidator>,
    },
    Partial {
        entries: Vec<CollectionEntry<T>>,
        validator: Option<PageValidator>,
    },
    Unchanged {
        validator: PageValidator,
    },
}

struct ListCx<C = NoCursor> {
    cursor: Option<C>,
}

impl<C> ListCx<C> {
    fn cursor(&self) -> Option<&C>;
    fn validator(&self) -> Option<&VersionToken>;
}

trait Cursor: Sized {
    fn encode(&self) -> Bytes;
    fn decode(bytes: &[u8]) -> Result<Self>;
}
```

`NoCursor` means there is no typed cursor for this list call. It does not imply
that the listing is complete. Completeness is declared by the `Collection`
variant: `Complete` is exhaustive for the provider's current knowledge, while
`Partial` is intentionally open or truncated with no cursor. The host only
stores encoded cursor bytes. It must not inspect provider cursor meaning.

The entry constructors are the whole preloading API for child objects:

```rust
CollectionEntry::fresh(key, object, canonical_bytes)
CollectionEntry::computed(key, files)
CollectionEntry::key(key)
```

Use `fresh` only when the list payload satisfies the child object's canonical
contract. Use `computed` for shallow list fields that can populate computed
leaves but not the canonical object. Use `key` for discovery only.

Collection outcomes:

- `Collection::complete(entries)` means the provider is returning the complete
  listing it knows at that instant.
- `Collection::page(entries).next(cursor)` means more entries are reachable and
  worth fetching.
- `Collection::partial(entries)` means the listing is intentionally open or
  truncated, with no cursor. Lookup remains authoritative for navigable names.
- `Collection::unchanged(validator)` means the validator of the call still
  matches its page: the cached page stands, with `validator` from then on,
  such as the validator of the call with the interval of its upstream.

Each page may carry the validator of its upstream answer with
`.with_validator(PageValidator { token, interval })`: the `ETag` of the page, or
a token that also holds what asking for the page again needs, with the seconds
of the interval the upstream asks between requests. The host gives the page a
deadline from that interval, three seconds without one, and once the page is
past it, the next listing asks for that page again with its cursor and with
the token as `ListCx::validator()`. A conditional request answers that call:
`RequestBuilder::page` maps `304` to `PageAnswer::Unchanged` and `200` to the
decoded body with its `ETag`. A raw dir handler does the same with
`DirListing::with_validator`, `DirListing::unchanged`, and `DirCx::version()`.
A page without a validator keeps the freshness of a cached listing.

No next cursor is not proof of completeness. A provider may return a short
terminal page as `Partial` when the upstream search window, cap, or domain is
not authoritative. GitHub's search cap, Linear's item cap, DNS's unbounded
domain space, and arXiv category pages all need these distinctions. A cursor
resumes a list; it does not prove freshness, existence, or completeness. A

Host pagination controls stay host-owned. Providers return typed cursors only.
The host synthesizes reserved `@next` and `@all` control entries when a real
resume cursor exists, persists those controls with cached dirents, and treats
reading a control file as a host action that advances or drains the listing.
Provider listings must not emit names reserved under the host `@` namespace.

## Feeds

Some upstreams report their changes since a token (GitHub's issues `since`,
event streams). For those, one feed per object keeps the facts of its fed faces
in the host projection, so a check of the feed answers for many paths at once:

```rust
r.object::<Repo>("/{owner}/{repo}", |o| {
    o.dynamic();
    o.dir("issues/{filter}").fed()?;
    o.feed(Repo::feed).starts(["issues/{filter}"])?;
    Ok(())
})?;
```

A fed face (`.fed()` on a file face, a dir face, or the anchor itself) has no
handler: lookup, listing, and read answer with the claims of the feeds of its
ancestry, and the host serves the path from its projection once the answering
feed has answered. The feed method gets a `FeedCx` with the anchor path the
host asked for, which the paths of its facts extend even where the key
normalizes a capture, the key, the token the
host kept, the declared starting path that started the answer, the projection
listings of its subtree (`FeedCx::listing`), and writers for the facts of the
part: `file` for a stable inline file, `dir` with a `ListingWrite`, and
`remove`. It answers `Feed::unchanged()` or `Feed::part(token)`, with `more`
while its answer continues and `inner::<O>(key, token)` for the tokens of inner
feeds, such as an item from a page of rows.

## Object load prefetch and effects

Collections are not the only place where preloading happens. Sometimes reading
one object requires fetching a payload that already contains sibling objects or
related files. Oura is the canonical example: one date-range response can
materialize the requested day plus neighboring day canonicals.

The object load result should be able to carry typed preloads:

```rust
Load::fresh(value, canonical)
    .preload_object(ObjectEntry::fresh(sibling_key, sibling, sibling_canonical))
    .preload_file(sibling_path, attrs)
```

This replaces hand-built raw effect plumbing at provider call sites. The effect
channel still exists below the SDK, but provider authors should return domain
objects, object entries, files, and invalidations. If a preloaded file needs
parent directories, the SDK can compute those directory effects from the path.

Events and invalidation remain first-class:

```rust
#[omnifs_sdk::provider(events(timer(60, Self::on_tick)))]
impl Provider {
    async fn on_tick(cx: Cx<State>) -> Result<Invalidation> {
        Ok(Invalidation::new()
            .listing_path("/scoped/item")
            .object::<Item>(&ItemKey::open(7)))
    }
}
```

Provider-local state is allowed when it is a routing or discovery catalog, not a
data cache. Kubernetes discovery and DB's known table set are examples. Provider
side LRUs, TTL caches, and memoized upstream results duplicate host caching and
should not exist.

## Direct read sibling preloads

A direct full-file read may also preload sibling files when the same upstream
payload already contains their bytes. This does not create object canonicals and
does not make the route object-shaped. It is the right shape for Docker
`inspect.json` projecting `state` and `summary.txt`.

The preload channel accepts only file sources that lower to view projections:
inline bytes and deferred reads. `Body`, `Ranged`, and `Blob` siblings do not
lower into project effects; they must be served by their own read, stream, or
blob face.

## Aliases and facets

Aliases are first-class because providers commonly expose the same object in
more than one place.

```rust
let paper = r.object::<Paper>("/papers/{paper}/{version}", |o| {
    o.dynamic();
    o.file("paper.atom").canonical::<Atom>()?;
    o.file("paper.pdf").blob(Paper::pdf)?;
    Ok(())
})?;

// The same Paper, also reachable under each of its categories.
r.alias("/categories/{category}/papers/{paper}/{version}", &paper)?;
```

An alias maps to the same object identity or to an explicit key conversion. If
it needs independent retrieval, it is not an alias.

Use facets for route context that must not split object identity: filters,
aliases, version selectors, and team aliases. A GitHub issue under
`issues/open/7` and `issues/all/7` is the same object if `filter` is a facet.
The facts of a fed object live once, under its anchor with its facets at their
first choice, and the host serves every other facet from them.

## Errors and control results

The object model must preserve current provider error semantics:

- parse rejection means impossible path syntax and route fallthrough;
- `Load::NotFound` means a valid object identity was absent upstream;
- `Load::Unchanged` means a conditional validator matched and cached canonical
  bytes are still usable;
- an `unchanged` listing answers only a call that carries the validator of a
  cached page;
- rate limit errors carry retry-after information when available;
- `InvalidInput` is for malformed provider input, not missing upstream data.

These are part of the SDK contract, not incidental handler details.

## Registration checks

The SDK should fail fast at registration time when the tree is incoherent.
Useful errors beat clever builders.

Reject:

- two contributors creating the same leaf;
- fixed object metadata colliding with collection child names;
- a collection path that cannot compute the target object key;
- a paged collection whose cursor type cannot round-trip through opaque bytes;
- an alias missing captures required by the target key;
- a feed starting path that names no fed face of its object's subtree;
- a fed face without a feed in its ancestry;
- path captures unused by the key or by route facets;
- an object implementation that tries to register hidden paths;
- `Live` on a non-stream face;
- eager computed leaves that cannot be represented as inline bytes;
- a canonical or representation face without a content type;
- a direct, blob, or stream face that lies about size or stability.

Error messages should name both contributors and the exact path.

Router compilation runs at component initialization: a route tree that fails
any check above aborts the provider's `initialize` with an error naming the
offending path. The gate is the host integration test
`all_providers_initialize_and_compile`
(`crates/omnifs-engine/tests/runtime_test.rs`); per repo convention providers
carry no in-crate route tests.

## Provider target shapes

### GitHub

GitHub is object-shaped with composed relative faces. Owner, repository, issue,
pull request, comment, and workflow run are objects. `/{owner}` is both the
`Owner` object directory and the directory that lists `Repo` children. The
repository object declares fed directories for issues and pulls, a relative
collection directory for workflow runs, and the `repo` tree handoff.

The feed of the repository keeps its issues, pull requests, and comments: the
`title`, `body`, `state`, and `user` of each item under `{kind}/all/`, which its
entry in `{kind}/open/` shares while it is open, and the `author` and `body.md`
of each comment. Its first full listing takes the rows of
`GET /repos/{owner}/{repo}/issues` by `updated_at`, then the comments of the
repository, then the GitHub GraphQL counts of the comments of each item, where
an item with another count takes the ids of its comments. After it, each answer
past the deadline is a check of each page of the rows since the time of the last
row, the first with the `ETag` of that time, and of the counts, where another
count of a kind takes the numbers of its items, and an item that GitHub GraphQL
cannot resolve leaves. Before the first
full listing ends, the feed of an item answers for it from
`GET /repos/{owner}/{repo}/issues/{number}` and takes its comments at the first
read of its `comments/`.

A collection stores child canonical bytes only when the list payload satisfies
the child object's canonical contract. Shallow fields can be eager computed
preloads. Large bodies should be lazy. Pull diffs should be blob-backed file
faces because the important property is host-resident bytes with exact size,
content type, and optional ETag/version evidence. Use a file-shaped object for a
diff only if the provider needs to decode the diff and expose computed faces. Use
direct only when every access really should call the upstream/provider again.
Comments are `Comment` objects projected as `comments/{comment_id}` directories
(one object keyed by the comment id, whose fed faces the feeds keep), not
indexed `comments/{idx}` files.

The pull diff is a blob face, not a direct file. The exact return type can move,
but the behavior must preserve host-resident bytes:

```rust
async fn diff(cx: Cx<GitHub>, key: PullRequestKey) -> Result<BlobFile<Diff>> {
    let blob = cx
        .endpoint(GitHubApi)
        .get(format!("/repos/{}/pulls/{}", key.repo(), key.number()))
        .header("Accept", "application/vnd.github.diff")
        .into_blob()
        .fetch()
        .await?;

    Ok(BlobFile::new(blob.id)
        .size(FileSize::Exact(blob.size))
        .content_type("text/x-diff"))
}
```

GitHub action logs are different in the current provider: the endpoint returns a
zip and the provider flattens it into text in guest memory. That can remain a
direct transformed file. If the log surface becomes a raw downloadable archive
or an extracted tree, model it as a blob or tree face instead of a direct file.

### arXiv

arXiv is object-shaped around papers and versions. The canonical paper bytes are
Atom. JSON, PDF, and source archives are file faces on the selected paper
version. Category browsing is a typed cursor collection of paper-version
objects.

PDFs and source tarballs are blob-backed file faces, not inline canonical object
bytes. A source archive remains an ordinary blob-backed file; if a directory/tree
face is needed later, it requires an explicitly designed provider-side path.

```text
/papers/{paper}/@latest/{paper.atom,paper.json,paper.pdf,source.tar.gz}
/papers/{paper}/vN/{paper.atom,paper.json,paper.pdf,source.tar.gz}
/categories/{category}/papers/{paper}/@latest/...
/categories/{category}/papers/{paper}/vN/...
```

`@latest` is dynamic. Numbered versions are stable. The version selector is a
route facet, not part of paper identity. PDF and source archives are file faces
with their own blob byte source, exact size, and stability inherited from the
selected version.

### DB

DB is route-shaped over a local backend, not canonical-cache shaped. The database
file is already the local source of truth, so metadata, table metadata, field
leaves, and samples are direct file faces over SQLite. The table-name
admissibility set is a current routing compromise: it prevents synthetic missing
table anchors when the local table universe is already known at provider start.

DB's `read_only` config and preopen mode are authority and mutability policy,
not stability. `sample.json` is dynamic, exact-size, and read-only, but the
current provider serves it as a whole-file body projection rather than a ranged
stream.

### DNS

DNS answers are direct dynamic file faces. Domain and resolver captures are
validated path segments, and record files are direct query leaves. Resolver
policy owns default versus named resolver selection. There is no fake DNS
canonical cache.

DNS needs open directories because the domain universe and reverse lookup space
are intentionally unbounded. Literal route precedence and capture validation
must remain visible in the route table. Aggregate leaves such as `all` may own
multi-callout fanout and partial-success error folding; the object model must
not assume one file read is all-or-nothing across all upstream callouts.

### Docker

Docker is route-shaped over operational state, not stable canonical state.
Containers, images, Compose projects, and Compose services are dynamic route
anchors. System files, lists, logs, and status leaves are fresh file faces.
Containers carry no canonical bytes, so by-name, by-id, running, and stopped are
keyed directories of plain file routes that share the same handlers, not object
aliases; Docker holds no object identities.

Docker should not emit canonical object-cache entries unless an upstream
response becomes a replayable object contract. Docker daemon access is authority
declared by config/manifest, not something route declarations can grant.

### Kubernetes

Kubernetes uses object semantics for API resources. Namespaces, resource types,
namespaced resources, cluster resources, pods, containers, and log streams are
all visible in the route API. Resource manifests are canonical JSON objects with
YAML representations and computed status leaves. `resourceVersion` is validator
evidence when present.

Events are direct sidecar file faces on resources. Pod logs are stream faces
under pod directories and may be `Live` with unknown size. Discovery state is a
provider-local routing catalog, not a host-cache replacement. The provider's
local proxy/socket endpoint and cluster credentials are manifest/config
authority, not route behavior.

Discovery owns resource admissibility and filesystem naming. It filters
subresources, requires both `get` and `list`, keeps core plurals bare, always
names grouped resources as `<plural>.<group>`, prefers preferred group versions,
and can hide empty resource types. Partial discovery produces open listings,
retains non-authoritative source failures, and retries on the next operation;
complete discovery is exhaustive and cached. Do not flatten this into a generic
collection rule; it is provider-local routing state backed by Kubernetes
discovery.

### Linear

Linear is object-shaped around teams, filters, and issues. Team and filter
directories compose with typed cursor issue collections. Issue identity is the
Linear identifier; team and filter captures are route facets that validate alias
context. List payloads may attach shallow computed files eagerly when they carry
those fields.

Linear GraphQL issue loads can produce validators from issue version fields even
when the upstream API cannot honor conditional requests. The model must allow a
provider to supply version evidence without pretending that `Load::Unchanged`
is possible for that upstream call.

### Oura

Oura is object-shaped around daily collection documents. A day directory lists a
finite collection enum whose path segments already include `.json`, such as
`daily_sleep.json` and `heart_rate.json`. Each `{day}/{collection}` file is a
canonical JSON object face for that day and collection.

Oura's range endpoints make object-load prefetch mandatory: reading one day can
fetch a window around that day and store neighboring daily collection canonicals
with exact sizes and validators. Some time-series collections, such as heart
rate and ring battery level, are object-shaped daily collection files using
datetime range partitioning. Use direct time-series faces only when a collection
cannot be partitioned into replayable daily canonicals.

### Test provider

The test provider is a conformance fixture. It should exercise scoped
invalidation, deferred reads, ranged files, live files, paged directories,
partial/open listings, aliases, collection preloading,
object-load prefetch, object feeds with fed faces, registration errors, rate
limits, and tree handoff. It should not be rewritten to match product provider
aesthetics.

## Cross-provider rules

- Keep the main tree in `start()`. Reusable object handles are fine; hidden
  registration helpers are not.
- Keys carry identity and route context, not behavior. Behavior lives on the
  object type.
- Path captures validate syntax. Existence normally belongs to listing, object
  retrieval, or backend lookup.
- Cursors are typed in provider code and opaque bytes to the host. They resume a
  list; they do not prove freshness, existence, or completeness.
- The host owns pagination controls and the reserved `@` namespace. Providers
  return cursors, not `@next` or `@all` entries.
- Cache only complete canonical bytes. A cache entry proves what was stored, not
  what is current upstream.
- Canonical bytes may be raw upstream bytes or deterministic logical-object
  bytes. They must be replayable by `decode`; they must not be convenience
  serialization of whatever Rust struct is easiest to persist.
- Mutability, stability, and authority are separate axes. Do not use one to
  smuggle another.
- Prefer object methods over one-caller computed-file helpers.
- Keep raw path calls only while the object API cannot express a shape. Delete
  them when the object face exists.
- Do not add macro attributes, builder fields, or generic parameters for values
  the type system or the path already names.
- Do not introduce `node`, `presentation`, or another umbrella noun. Provider
  authors see objects, files, directories, collections, aliases, streams, and
  trees.
- Keep provider-local state limited to routing/admissibility/discovery catalogs
  or operation state. Do not add provider-side data caches to compensate for a
  missing SDK expression.
- Update this document when the public SDK shape changes. Do not append old
  iteration notes.
