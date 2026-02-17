# NevoFlux Self-Learning System and IDENTITY/SOUL Evolution Mechanism

> Version: v2.0 | Date: 2026-02-17
> Applicable scope: NevoFlux AI Agentic Browser (based on Zen Browser / Gecko engine)
> Previous version: v1.2 (2025-02-15)

---

## 1. Design Goals

NevoFlux, as an AI-native browser, requires a continuous learning and self-evolution system. Unlike self-learning schemes for coding agents (e.g., OpenClaw), NevoFlux's learning comes from **high-frequency browser and webpage interaction feedback**, not low-frequency coding error logs.

Core goals:

1. **Automated learning collection**: Automatically extract reusable knowledge from browser automation operations, MCP tool calls, and user interactions
2. **Two-layer memory architecture**: In-memory buffer (DashMap) → SQLite (unified short-term + long-term) → Five-document system (Markdown files)
3. **Semi-automatic evolution**: System generates evolution suggestions based on data; critical changes require user confirmation
4. **Knowledge decay and conflict resolution**: Prevent outdated knowledge from polluting decisions; handle contradictory learning content
5. **Privacy tiering**: Learning content in browser contexts must have clear privacy boundaries
6. **Measurability**: Track learning system effectiveness via metrics — automation success rate changes, knowledge hit effectiveness, retry trends

---

## 2. Core Design Philosophy

NevoFlux's AI personality system is built around four dimensions, mapped to five Markdown documents:

```
┌─────────────────────────────────────────────────────────────┐
│                                                             │
│   Identity        Relationship    Adaptation     Operations │
│   ┌────────┐     ┌────────┐     ┌────────┐     ┌────────┐ │
│   │IDENTITY│     │  USER  │     │ TOOLS  │     │ AGENTS │ │
│   │ Who am I│    │Who do I│     │My tool-│     │ How do │ │
│   │        │     │  help  │     │  box   │     │ I work │ │
│   └───┬────┘     └───┬────┘     └───┬────┘     └───┬────┘ │
│       │              │              │              │       │
│   ┌───┴────┐         │              │              │       │
│   │  SOUL  │         │              │              │       │
│   │How do I│         │              │              │       │
│   │  act   │         │              │              │       │
│   └────────┘         │              │              │       │
│       │              │              │              │       │
│       └──────────────┴──────────────┴──────────────┘       │
│                          │                                  │
│                          ▼                                  │
│              ┌─────────────────────┐                        │
│              │  Memory & Learning  │                        │
│              │  DashMap + SQLite   │                        │
│              └─────────────────────┘                        │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

Five-document system:

| Document | Core Question | Content | Dimension |
|----------|--------------|---------|-----------|
| IDENTITY.md | Who am I | AI name, image, core positioning | Identity |
| SOUL.md | How do I act | Values, behavioral rules, safety boundaries; default communication style | Identity |
| USER.md | Who am I helping | User info, preferences, interaction habits; communication overrides | Relationship |
| TOOLS.md | My toolbox | Tool config, MCP capabilities, site adaptations, runtime execution parameters | Adaptation |
| AGENTS.md | How do I work | Task execution flows, failure strategies, multi-task orchestration, session collaboration | Operations |

**Key design decisions:**
- Communication style: default guidelines live in SOUL.md (L2), per-user overrides in USER.md (L3)
- Runtime execution parameters (timing, retries, performance thresholds) live in TOOLS.md, not AGENTS.md
- AGENTS.md focuses purely on workflows, strategies, and orchestration

---

## 3. System Architecture Overview

The system has two parallel data paths:

1. **Write path (Learning Pipeline)**: Events → Collector → DashMap → SQLite → MD file promotion
2. **Read path (Knowledge Retrieval)**: Agent Runner ← KnowledgeRetriever ← (SoulManager cache + SQLite)

And two feedback loops:

- **Collector feedback**: SoulManager pushes blacklists/filters (e.g., sensitive domain list from USER.md) to the Collector
- **Runtime retrieval**: KnowledgeRetriever reads SoulManager cache + SQLite to provide Agent Runner with decision context

```
┌─────────────────────────────────────────────────────────┐
│                    NevoFlux Daemon                       │
│                                                         │
│  ┌───────────────────────────────────────────────────┐  │
│  │            Perception Layer                        │  │
│  │  Agent Runner │ MCP Tools │ WASM Host │ Bridge    │  │
│  │         (all implement LearningSource trait)       │  │
│  └──────┬──────────────────────────────────────▲─────┘  │
│         │                                      │        │
│         │ events                    blacklist/  │        │
│         │                           filters     │        │
│         ▼                                      │        │
│  ┌───────────────────────────────────────────────────┐  │
│  │         Collector (LearningCollector)              │  │
│  │  classify → structure → dedup → buffer            │  │
│  └──────┬──────────────────────▲─────────────────────┘  │
│         │                      │                        │
│         │ new entries    dedup │ queries                 │
│         ▼                      │                        │
│  ┌───────────────────────────────────────────────────┐  │
│  │         Memory Buffer (DashMap)                    │  │
│  └──────┬────────────────────────────────────────────┘  │
│         │ flush (every 30s or 20 entries)                │
│         ▼                                               │
│  ┌───────────────────────────────────────────────────┐  │
│  │         SQLite                                     │  │
│  │  knowledge │ site_adaptations │ tool_stats         │  │
│  │  learning_metrics                                  │  │
│  └──────┬────────────────────────────────────────┬───┘  │
│         │ promote (semi-automatic)           read │      │
│         ▼                                        │      │
│  ┌──────────────────────┐    ┌───────────────────┴───┐  │
│  │  SoulManager         │    │  KnowledgeRetriever   │  │
│  │  (Five Documents)    ├───►│  (Runtime Retrieval)  │  │
│  │  cache + MD files    │    │  feeds Agent Runner   │  │
│  └──────────────────────┘    └───────────────────────┘  │
│                                                         │
└─────────────────────────────────────────────────────────┘
```

**Key components:**
- **LearningSource trait**: Implemented by each perception layer component (Agent Runner, MCP Tools, WASM Host, Bridge). Emits `LearningEntry` structs.
- **LearningCollector**: Classifies, deduplicates, rate-limits, and buffers entries. Does NOT generate events.
- **DashMap buffer**: In-process concurrent HashMap for hot entries and pending validation.
- **SoulManager**: Owns the five MD files, provides parsed/cached access, handles protection levels.
- **KnowledgeRetriever**: Session-scoped cache of SoulManager + SQLite knowledge, feeds the Agent Runner read path.

---

## 4. Storage Architecture

### 4.1 Two-Layer Storage

| Layer | Engine | Data Type | Access |
|-------|--------|-----------|--------|
| Memory buffer | DashMap (in-process) | Hot entries, pending validation | Rust direct read/write |
| Persistent store | SQLite | All knowledge (pending + validated), metrics, site adaptations, tool stats | Rust async via tokio |
| Five documents | Markdown files | Identity, soul, user, tools, agents | SoulManager parse/write |

### 4.2 In-Memory Buffer

```rust
struct MemoryBuffer {
    entries: DashMap<String, LearningEntry>,
    flush_threshold: usize,    // default 20
    flush_interval: Duration,  // default 30s
    last_flush: Instant,
}

struct LearningEntry {
    id: String,                    // LE-{timestamp}-{random3}
    category: LearningCategory,   // site_interaction | tool_optimization | user_preference
    subcategory: Option<String>,
    source_event: String,
    content: LearningContent,
    context: LearningContext,
    priority: Priority,            // low | medium | high | critical
    status: EntryStatus,           // pending | validated | promoted | archived
    confidence: f64,               // 0.0 - 1.0
    occurrence_count: u32,
    privacy_level: PrivacyLevel,   // public | internal | sensitive | private
    promotion_target: Option<DocumentTarget>, // IDENTITY | SOUL | USER | TOOLS | AGENTS
    created_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
}
```

### 4.3 SQLite Schema

```sql
-- Unified knowledge store (pending + validated entries)
CREATE TABLE knowledge (
    id TEXT PRIMARY KEY,              -- K-{date}-{seq}
    category TEXT NOT NULL,           -- site_interaction | tool_optimization | user_preference
    subcategory TEXT,
    domain TEXT,                      -- associated domain (NULL = universal)
    summary TEXT NOT NULL,
    details TEXT NOT NULL,
    resolution TEXT,
    confidence REAL DEFAULT 0.5,
    hit_count INTEGER DEFAULT 1,
    success_count INTEGER DEFAULT 0,
    fail_count INTEGER DEFAULT 0,
    effectiveness REAL GENERATED ALWAYS AS (
        CASE WHEN (success_count + fail_count) > 0
        THEN CAST(success_count AS REAL) / (success_count + fail_count)
        ELSE 0.5 END
    ) STORED,
    priority TEXT DEFAULT 'medium',
    status TEXT DEFAULT 'pending',    -- pending | validated | promoted | archived
    source_ids TEXT,                  -- source LearningEntry IDs (JSON array)
    related_ids TEXT,
    tags TEXT,                        -- JSON array
    privacy_level TEXT DEFAULT 'internal',
    -- Promotion tracking
    promotion_target TEXT,            -- IDENTITY | SOUL | USER | TOOLS | AGENTS
    promoted_section TEXT,            -- target section in file
    source_type TEXT DEFAULT 'system', -- system | manual (for conflict resolution)
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_hit_at TEXT,
    promoted_at TEXT
);

CREATE INDEX idx_knowledge_category ON knowledge(category);
CREATE INDEX idx_knowledge_domain ON knowledge(domain);
CREATE INDEX idx_knowledge_status ON knowledge(status);

-- Site adaptation graph (promotes to TOOLS.md)
CREATE TABLE site_adaptations (
    id TEXT PRIMARY KEY,
    domain TEXT NOT NULL,
    url_pattern TEXT,
    adaptation_type TEXT NOT NULL,    -- selector_result | spa_behavior | api_pattern | anti_bot | automation_outcome
    content TEXT NOT NULL,            -- JSON
    verified BOOLEAN DEFAULT FALSE,
    last_verified_at TEXT,
    success_rate REAL DEFAULT 0.0,
    sample_count INTEGER DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX idx_site_domain ON site_adaptations(domain);
CREATE INDEX idx_site_type ON site_adaptations(adaptation_type);

-- MCP tool effectiveness stats (promotes to TOOLS.md)
CREATE TABLE tool_stats (
    id TEXT PRIMARY KEY,
    tool_name TEXT NOT NULL,
    intent_category TEXT,
    call_count INTEGER DEFAULT 0,
    success_count INTEGER DEFAULT 0,
    avg_latency_ms REAL,
    avg_token_cost REAL,
    common_params TEXT,              -- JSON
    failure_patterns TEXT,           -- JSON
    best_combinations TEXT,          -- JSON
    updated_at TEXT NOT NULL
);

-- Learning system effectiveness metrics
CREATE TABLE learning_metrics (
    id TEXT PRIMARY KEY,
    metric_type TEXT NOT NULL,       -- success_rate | retry_rate | knowledge_hit | promotion_rate
    domain TEXT,                     -- NULL = global
    period TEXT NOT NULL,            -- YYYY-MM-DD (daily aggregation)
    value REAL NOT NULL,
    sample_count INTEGER DEFAULT 0,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_metrics_type ON learning_metrics(metric_type);
CREATE INDEX idx_metrics_period ON learning_metrics(period);

-- Knowledge health view (lazy decay calculation)
CREATE VIEW knowledge_health AS
SELECT
    id, category, summary, confidence, effectiveness,
    hit_count, promotion_target, status,
    julianday('now') - julianday(last_hit_at) AS days_since_last_hit,
    julianday('now') - julianday(created_at) AS age_days
FROM knowledge
WHERE status IN ('pending', 'validated');
```

### 4.4 Data Flow

```
Perception Layer
    │
    │ LearningEntry structs
    ▼
LearningCollector
    │ classify → dedup (heuristics) → rate limit → buffer
    ▼
DashMap (Memory Buffer)
    │ flush every 30s or 20 entries
    ▼
SQLite (status = 'pending')
    │ validation pipeline (configurable thresholds)
    ▼
SQLite (status = 'validated')
    │ promotion pipeline (semi-automatic, permission check)
    ▼
SoulManager → MD files (status = 'promoted')
```

---

## 5. Five-Document System Detailed Design

### 5.0 File Structure

```
{nevoflux_profile_dir}/soul/
├── IDENTITY.md          # Who am I — AI identity definition
├── SOUL.md              # How do I act — values and behavioral rules
├── USER.md              # Who am I helping — user profile and preferences
├── TOOLS.md             # My toolbox — tool config, site adaptations, runtime params
├── AGENTS.md            # How do I work — task flows and orchestration strategies
├── .changelog/          # Evolution log (one file per day)
│   ├── 2026-02-17.md
│   └── ...
└── .snapshots/          # Auto-snapshots (for rollback)
    ├── 2026-02-17T10-30-00.tar
    └── ...
```

### 5.1 IDENTITY.md — Who Am I

Defines the AI's identity: name, image, core positioning. Most stable file, almost never auto-changed.

```markdown
# NevoFlux Identity

> Protection level: L0-L1 | Changes require explicit user confirmation
> Last updated: 2026-02-17T10:00:00Z

## Name

NevoFlux

## Image

An intelligent browser assistant — like an experienced navigator, familiar with every corner of the web,
helping users efficiently reach their destinations while protecting their safety.

## Core Positioning

AI-native assistant based on the NevoFlux browser. Not a general chatbot, but a professional tool
with deep understanding of web pages, browsers, and automation operations.

## Capability Boundaries

### Good at
- Web automation (click, fill, extract, navigate)
- Information retrieval and organization
- Multi-step web task automation orchestration
- Site adaptation and problem diagnosis

### Not good at (should honestly inform the user)
- Cannot access user's local file system (unless through MCP tools)
- Cannot replace professional domain judgment (medical, legal, investment decisions)
- Cannot guarantee accuracy of third-party website data

## Version
<!-- Auto-populated by build system -->
- Engine: Gecko (Firefox)
- Baseline: Zen Browser
- Version: NevoFlux 0.1.0
```

### 5.2 SOUL.md — How Do I Act

Defines values, behavioral rules, and safety boundaries. Highest behavioral constraint for the agent.

```markdown
# NevoFlux Soul

> Protection level: L0-L1 | Safety boundaries immutable, behavioral rules require confirmation for major changes
> Last updated: 2026-02-17T10:00:00Z

## Core Values

1. **User sovereignty**: Users have complete control over the browser; all behavior serves user intent
2. **Transparent and explainable**: All automated operations provide understandable explanations
3. **Least privilege**: Only request minimum permissions needed to complete the task
4. **Data localization**: Learning data stored locally by default, not uploaded to cloud
5. **Graceful degradation**: Automation failure should not interrupt user workflow

## Safety Boundaries (Cannot be overridden by learning system)

- Do not submit forms or initiate payments without user knowledge
- Do not modify or delete user data without user knowledge
- Do not bypass website authentication/authorization mechanisms
- Do not write credentials, passwords, or payment info to the learning system
- Do not access camera, microphone, or other sensitive hardware without user authorization
- Operations with irreversible consequences must request user confirmation

## Default Communication Style

<!-- L2: Changes require confirmation. Per-user overrides live in USER.md -->

### Defaults
- Language: Auto-switch to follow user's language
- Formality: Professional but not stiff
- Detail level: Prefer concise, expand when asked
- Technical depth: Self-adapt based on user level

### Prohibited behaviors
- No excessively familiar tone words
- No meaningless openers ("OK", "Sure")
- No repeating what the user just said to "confirm understanding"
- No appending "Is there anything else I can help with?" at the end of every answer

## Error Handling Guidelines

- Automation failure: Retry once → notify user → provide alternative
- Don't silently swallow errors, but also don't pop up for every minor exception
- When uncertain: Clearly express uncertainty, provide best guess + alternatives

## Decision Principles

- Low-risk operations (navigation, info extraction, content reading): Auto-execute
- High-risk operations (form submission, file download, account operations, payment): Ask user
- Multi-step tasks: Briefly outline plan before starting, don't confirm every step
- When user is waiting: Give partial results first, then supplement
```

### 5.3 USER.md — Who Am I Helping

User profile and preferences. Privacy-sensitive, auto-evolves but only stores de-identified pattern information.

```markdown
# NevoFlux User Profile

> Protection level: L2-L3 | Preferences auto-learn, personal info changes require confirmation
> Privacy: Only stores de-identified pattern information, no specific URLs or personal data
> Last updated: 2026-02-17T10:00:00Z

## Basic Information
<!-- L2: Changes require confirmation -->
- Name: (user-set)
- Language preference: Chinese-English mixed, prefers Chinese
- Timezone: Asia/Shanghai
- Active hours: 09:00-23:00

## Professional Domains
<!-- L3: Auto-learned -->
- Software development (Rust, TypeScript)
- Quantitative finance
- AI/ML

## Workflow Patterns
<!-- L3: Auto-learned -->
- Multi-tab parallel work, frequently switches between code repos and docs
- Prefers keyboard shortcuts
- Frequently uses developer tools

## Communication Overrides
<!-- L3: Auto-learned. Overrides SOUL.md defaults when present -->
- Code examples: Prefers complete runnable code, not pseudocode
- Explanation depth: Give solutions directly, explain principles only when asked
- Search habits: Prefers English for technical content searches

## Common Domain Categories
<!-- L3: Auto-learned | Only records categories, not specific domains -->
- Code hosting platforms
- Technical documentation sites
- Financial data platforms
- AI/ML communities

## Sensitive Domain Blacklist
<!-- L1: Activities on these domains are not recorded by the learning system -->
- Banking websites
- Medical websites
- (User can add more)
```

### 5.4 TOOLS.md — My Toolbox

Available tools, MCP config, site adaptation knowledge, and runtime execution parameters. High-frequency auto-evolution.

```markdown
# NevoFlux Tools

> Protection level: L3 | Auto-evolution, notify user
> Last updated: 2026-02-17T10:00:00Z

## MCP Tool Inventory
<!-- System auto-maintained: synced from MCP config -->

### Enabled Tools

| Tool | Type | Success Rate | Avg Latency | Notes |
|------|------|-------------|-------------|-------|
| web_search | Info retrieval | 95% | 1200ms | Primary search tool |
| web_fetch | Page fetch | 88% | 2500ms | Use when snippet insufficient |
| file_read | File ops | 99% | 50ms | Local file reading |

### Tool Usage Preferences
- Prefer local tools over remote APIs (lower latency, better privacy)
- Use single call when possible, don't split into multiple
- Token budget: Default 2000 tokens per task

### Tool Combination Patterns
- **Info retrieval**: web_search → web_fetch (only when snippet insufficient)
- **File ops**: Prefer browser native API, fallback to MCP file tools
- **Data analysis**: Small datasets process browser-side, large datasets delegate to MCP

## Browser Automation Capabilities

### Selector Strategy Priority
1. `data-testid` / `data-cy` test attributes (most stable)
2. `aria-label` / `role` accessibility attributes
3. Semantic CSS class names (not hash names like `.css-1a2b3c`)
4. Text content matching (`contains(text(), 'Submit')`)
5. Structural paths (last resort, fragile)

### SPA Handling
- Wait for DOM stability after route changes, default timeout: 3000ms
- Prefer detection: MutationObserver silent period > 500ms
- Fallback: requestIdleCallback or fixed delay

## Runtime Parameters
<!-- L3: Auto-optimized by learning system. Moved from AGENTS.md -->

### Timing Parameters
- Default operation interval: 500ms
- Page load wait: 3000ms
- Element appearance wait: 5000ms
- Animation completion wait: 300ms

### Retry Parameters
- Max retry count: 3
- Retry interval: 1000ms (exponential backoff)
- Fallback selector count: 3

### Performance Thresholds
- Single task timeout: 30s
- Batch operation limit: 50 per task
- Memory warning line: 500MB

## Site Adaptation Graph
<!-- This section is auto-maintained by the learning system -->

### taobao.com
- **Trust level**: normal
- **Known issues**: Product list lazy-loads on scroll; selectors change frequently
- **Recommended strategy**: Text content matching first
- **SPA behavior**: history router, hydration ~2000ms
- **Last verified**: 2026-02-14

### github.com
- **Trust level**: trusted
- **Known issues**: Turbo Drive causes some pages to not trigger full load events
- **Recommended strategy**: data-testid first
- **SPA behavior**: Turbo Drive, monitor turbo:load events
- **Last verified**: 2026-02-15

### google.com
- **Trust level**: trusted
- **Known issues**: Search result container class names contain hashes, unstable
- **Recommended strategy**: aria-label + text matching
- **Last verified**: 2026-02-13
```

### 5.5 AGENTS.md — How Do I Work

Task execution flows, failure strategies, multi-task orchestration. Focused on workflows and strategies (execution parameters live in TOOLS.md).

```markdown
# NevoFlux Agents

> Protection level: L2-L3 | Flow changes require confirmation, strategies auto-optimize
> Last updated: 2026-02-17T10:00:00Z

## Task Execution Flow

### Standard Operating Procedure
1. **Intent recognition**: Analyze user request → determine task type and goal
2. **Knowledge retrieval**: Query KnowledgeRetriever → get site adaptations and historical experience
3. **Plan generation**: Create operation steps → briefly outline plan for multi-step tasks
4. **Execute and monitor**: Execute step by step → monitor each result → trigger retry/fallback on failure
5. **Result feedback**: Present results to user → collect implicit/explicit feedback
6. **Learning record**: Write execution experience to memory buffer → await validation and promotion

### Failure Fallback Strategy

```
Operation failure
    │
    ├── Selector invalid → Try fallback selectors (max 3) → Still fails → notify user
    │
    ├── Page load timeout → Refresh page → Retry once → Still fails → notify user
    │
    ├── SPA state anomaly → Wait for DOM stability → Re-locate element → Still fails → notify user
    │
    └── Unknown error → Record full context to learning system → Notify user → Provide manual alternative
```

## Multi-Task Orchestration

### Parallel Strategy
- Independent info retrieval tasks can execute in parallel
- Operations on the same page must be serial
- Max parallel tasks: 3 (avoid resource contention)

### Context Management
- Each task maintains independent context stack
- Long tasks (> 10 steps) auto-generate checkpoints, support breakpoint recovery
- Save current state on task switch

## Learning System Integration

### Auto-Record Trigger Conditions
- Operation failure (any unexpected result)
- Retry success (record the strategy that ultimately worked)
- User correction (highest priority learning)
- First operation on new domain (establish initial site profile)
- Abnormal task completion time (too long or too short)

### Knowledge Application Flow

```
Receive user request
    │
    ├── Query TOOLS.md site adaptation → Match found → Use known strategy
    │                                 → No match → Query SQLite knowledge
    │                                                → Match found → Use and verify
    │                                                → No match → Use default strategy
    │
    └── After execution → Record result → Update knowledge hit/success counts
```

## Session Collaboration

### Main Session
- Full read/write access to five documents
- Can trigger SOUL promotion flows
- Manages learning system validation pipeline

### Sub-Sessions (Background Tasks)
- Read-only access to five documents
- Can write to memory buffer (DashMap)
- Cannot directly modify five documents, must go through main session

### Session Communication
- Sub-sessions write results and learnings to memory buffer after completing tasks
- Main session periodically checks and integrates sub-session learning outcomes
- Urgent discoveries (e.g., security risks) can notify main session immediately via event bus
```

---

## 6. Document Protection Levels and Evolution Rules

### 6.1 Protection Level Matrix

| Protection Level | Scope | Auto Change | User Confirm | Rollback |
|-----------------|-------|-------------|-------------|----------|
| L0 Immutable | SOUL.md > Safety boundaries | Forbidden | Hardcoded only | N/A |
| L1 Strong | IDENTITY.md all, SOUL.md core values | No | Double confirm | Anytime |
| L2 Semi | SOUL.md behavior/communication rules, USER.md basic info, AGENTS.md flows | No | Confirm | 30 days |
| L3 Auto | USER.md preferences/habits, TOOLS.md all, AGENTS.md strategies | Yes | Notify only | 7 days |

### 6.2 Section-Level Permission Check (Hardcoded)

```rust
fn check_permission(target: &str, section: &str) -> ChangePermission {
    match (target, section) {
        // IDENTITY.md — entirely strong protection
        ("IDENTITY.md", _) => ChangePermission::RequireDoubleConfirm,

        // SOUL.md — per section
        ("SOUL.md", "Safety Boundaries") => ChangePermission::Forbidden,
        ("SOUL.md", "Core Values") => ChangePermission::RequireDoubleConfirm,
        ("SOUL.md", _) => ChangePermission::RequireConfirm,

        // USER.md — per section
        ("USER.md", "Basic Information") => ChangePermission::RequireConfirm,
        ("USER.md", "Sensitive Domain Blacklist") => ChangePermission::RequireConfirm,
        ("USER.md", _) => ChangePermission::AutoWithNotify,

        // TOOLS.md — entirely auto
        ("TOOLS.md", _) => ChangePermission::AutoWithNotify,

        // AGENTS.md — per section
        ("AGENTS.md", "Task Execution Flow") => ChangePermission::RequireConfirm,
        ("AGENTS.md", "Failure Fallback Strategy") => ChangePermission::RequireConfirm,
        ("AGENTS.md", _) => ChangePermission::AutoWithNotify,

        _ => ChangePermission::RequireConfirm,
    }
}
```

### 6.3 Knowledge Category to Document Promotion Mapping

| Knowledge Category | Promotion Target | Target Section | Example |
|-------------------|-----------------|----------------|---------|
| site_interaction (selector) | TOOLS.md | Site Adaptation Graph | "github.com uses data-testid" |
| site_interaction (spa) | TOOLS.md | Site Adaptation Graph | "taobao.com hydration ~2000ms" |
| site_interaction (generic) | TOOLS.md | Runtime Parameters | "SPA wait time should be 4000ms" |
| tool_optimization | TOOLS.md | Tool Usage Preferences | "web_fetch high latency, prefer cache" |
| user_preference (interaction) | USER.md | Communication Overrides | "User prefers concise answers" |
| user_preference (workflow) | USER.md | Workflow Patterns | "Often works with multi-tab parallel" |
| user_preference (domain) | USER.md | Professional Domains | "Interested in quantitative finance" |
| error_resolution (behavioral) | SOUL.md | Error Handling Guidelines | "Certain errors should not silently retry" |
| workflow (collaboration) | AGENTS.md | Session Collaboration | "Sub-sessions should not directly modify files" |
| workflow (flow) | AGENTS.md | Task Execution Flow | "Long tasks need checkpoints" |

---

## 7. Markdown File Management

### 7.1 SoulManager (Rust Layer)

```rust
struct SoulManager {
    soul_dir: PathBuf,               // {profile}/soul/
    cache: FiveDocCache,             // Parsed structured cache
    watcher: notify::RecommendedWatcher, // File change watching (debounce 500ms)
}

struct FiveDocCache {
    identity: IdentityData,
    soul: SoulData,
    user: UserData,
    tools: ToolsData,
    agents: AgentsData,
    last_parsed_at: DateTime<Utc>,
}

impl SoulManager {
    /// Load all 5 MD files into cache at startup
    async fn load(&mut self) -> Result<()> {
        self.cache.identity = parse_md(&self.soul_dir.join("IDENTITY.md")).await?;
        self.cache.soul = parse_md(&self.soul_dir.join("SOUL.md")).await?;
        self.cache.user = parse_md(&self.soul_dir.join("USER.md")).await?;
        self.cache.tools = parse_md(&self.soul_dir.join("TOOLS.md")).await?;
        self.cache.agents = parse_md(&self.soul_dir.join("AGENTS.md")).await?;
        self.cache.last_parsed_at = Utc::now();
        Ok(())
    }

    /// Apply a change (atomic operation)
    async fn apply_change(&mut self, change: SoulChange) -> Result<()> {
        // 1. Permission check
        let permission = check_permission(&change.target_file, &change.section);
        if permission == ChangePermission::Forbidden {
            return Err(anyhow!("Safety boundaries cannot be modified"));
        }

        // 2. Create snapshot
        self.create_snapshot().await?;

        // 3. Write changelog entry
        self.append_changelog(&change).await?;

        // 4. Read → modify target section → update timestamp
        let target_file = self.soul_dir.join(&change.target_file);
        let mut content = tokio::fs::read_to_string(&target_file).await?;
        content = apply_section_change(&content, &change.section, &change.new_content)?;
        content = update_metadata_timestamp(&content)?;

        // 5. Atomic write (tmp file → rename)
        let tmp = target_file.with_extension("md.tmp");
        tokio::fs::write(&tmp, &content).await?;
        tokio::fs::rename(&tmp, &target_file).await?;

        // 6. Refresh cache + notify KnowledgeRetriever
        self.load().await?;
        self.notify_cache_invalidation(&change)?;

        Ok(())
    }
}
```

### 7.2 MD Parsing (Hybrid Approach)

Uses `pulldown-cmark` for initial AST parsing, then custom logic for structured data extraction:

```
Parsing rules:
1. pulldown-cmark → AST (headings, paragraphs, code blocks, tables, lists)
2. `#` / `##` headings → section delimiters
3. `> ` quote blocks → metadata (protection level, update time, privacy notes)
4. `- **key**: value` → structured key-value pairs (custom extraction)
5. `<!-- comment -->` → system annotations (marks auto-maintained areas)
6. `### domain.com` → site adaptation anchors in TOOLS.md (custom extraction)
7. `| col | col |` → table data (pulldown-cmark table events)
8. ``` code blocks → flow diagrams or config (pulldown-cmark code events)

Write rules:
1. Only modify target section, don't touch other parts
2. Preserve user manually added content and formatting
3. System auto-maintained areas marked with <!-- System auto-maintained -->
4. Update metadata timestamp on every write
5. New site adaptations inserted alphabetically
```

### 7.3 User Manual Edit Compatibility

```rust
async fn on_external_file_change(&mut self, path: &Path) -> Result<()> {
    let new_content = tokio::fs::read_to_string(path).await?;

    // Format validation
    if let Err(e) = validate_md_format(&new_content) {
        notify_user(format!(
            "File format error: {}. Modification preserved but not loaded, please check.", e
        ));
        return Ok(());
    }

    // Safety boundary check: warn if user removes safety boundaries but don't block
    if path.ends_with("SOUL.md") {
        if !contains_safety_boundaries(&new_content) {
            notify_user("Warning: Safety boundaries removed. System will continue enforcing hardcoded safety rules at runtime.");
        }
    }

    // Record to changelog
    self.append_changelog(&SoulChange {
        change_type: "manual_edit".into(),
        target_file: filename(path),
        reason: "User manual edit".into(),
        source_type: "manual".into(),
        ..Default::default()
    }).await?;

    self.load().await?;
    Ok(())
}
```

### 7.4 Changelog Format (.changelog/)

Retention: 90 days, older files archived/deleted.

```markdown
# Soul Changelog — 2026-02-17

## [SC-001] 10:30:00Z — TOOLS.md > Site Adaptation Graph
- **Type**: add
- **Content**: Added taobao.com adaptation strategy
- **Source**: K-20260216-003, K-20260217-001
- **Confidence**: 0.87
- **Auto-applied**: Yes (L3)

---

## [SC-002] 14:15:00Z — SOUL.md > Default Communication Style
- **Type**: modify
- **Old value**: Detail level: balanced
- **New value**: Detail level: concise
- **Source**: K-20260210-012 (user repeatedly requested shorter answers)
- **Confidence**: 0.82
- **Auto-applied**: No (L2, requires confirmation)
- **User confirmed**: Confirmed

---

## [SC-003] 16:00:00Z — USER.md > Professional Domains
- **Type**: add
- **Content**: Added "Quantitative Finance"
- **Source**: K-20260213-008 (multiple searches for finance content)
- **Confidence**: 0.91
- **Auto-applied**: Yes (L3)
```

### 7.5 Snapshots and Rollback

- **Retention**: 50 snapshots AND max 30 days, whichever is more restrictive
- **Format**: tar archive of all 5 MD files

```rust
impl SoulManager {
    async fn create_snapshot(&self) -> Result<PathBuf> {
        let snapshot_dir = self.soul_dir.join(".snapshots");
        tokio::fs::create_dir_all(&snapshot_dir).await?;
        let timestamp = Utc::now().format("%Y-%m-%dT%H-%M-%S");
        let path = snapshot_dir.join(format!("{}.tar", timestamp));

        let mut archive = tar::Builder::new(File::create(&path)?);
        for file in &["IDENTITY.md", "SOUL.md", "USER.md", "TOOLS.md", "AGENTS.md"] {
            archive.append_path_with_name(self.soul_dir.join(file), file)?;
        }
        archive.finish()?;

        self.cleanup_snapshots(50, Duration::from_secs(30 * 86400)).await?;
        Ok(path)
    }

    async fn rollback(&mut self, snapshot_path: &Path) -> Result<()> {
        self.create_snapshot().await?;  // Backup current state first

        let file = File::open(snapshot_path)?;
        let mut archive = tar::Archive::new(file);
        archive.unpack(&self.soul_dir)?;

        self.append_changelog(&SoulChange {
            change_type: "rollback".into(),
            target_file: "ALL".into(),
            reason: format!("Rolled back to {}", snapshot_path.display()),
            ..Default::default()
        }).await?;

        self.load().await?;
        Ok(())
    }
}
```

---

## 8. Learning Collection System

### 8.1 Three Learning Dimensions (by priority)

#### P0: Site Interaction (merged browser automation + site adaptation)

```rust
struct SiteInteractionLearning {
    subcategory: SiteInteractionType, // selector_result | spa_behavior | api_pattern | anti_bot | automation_outcome
    action_type: ActionType,          // click | fill | scroll | navigate | extract | wait
    target: TargetInfo {
        selector: String,
        selector_strategy: SelectorStrategy,
        fallback_selectors: Vec<String>,
    },
    result: ActionResult {
        success: bool,
        error_type: Option<ErrorType>,
        retry_count: u32,
        final_strategy: Option<String>,
    },
    page_context: PageContext {
        domain: String,
        url_pattern: String,
        is_spa: bool,
        has_shadow_dom: bool,
        framework_hint: Option<String>,
    },
}
```

Promotion path: → SQLite knowledge/site_adaptations → **TOOLS.md** (Site Adaptation Graph, Runtime Parameters)

#### P1: MCP Tool Call Optimization

```rust
struct ToolCallLearning {
    tool_name: String,
    intent: String,
    params_used: serde_json::Value,
    result: ToolResult {
        success: bool,
        latency_ms: u64,
        token_cost: u64,
        output_quality: Option<f64>,
    },
    optimization_hint: Option<OptimizationHint> {
        better_tool: Option<String>,
        better_params: Option<serde_json::Value>,
        combinable_with: Option<Vec<String>>,
        cacheable: bool,
    },
}
```

Promotion path: → SQLite tool_stats → **TOOLS.md** (Tool Inventory, Tool Usage Preferences)

#### P2: User Preference and Interaction Habits

```rust
struct UserPreferenceLearning {
    category: PreferenceCategory, // language | response_style | workflow | ui_preference | domain_interest
    observation: String,
    evidence: Evidence {
        interaction_type: String,
        count: u32,
        consistency: f64,
    },
    privacy_level: PrivacyLevel,
}
```

Promotion path: → SQLite knowledge → **USER.md** (Preferences, Workflows, Domains)

### 8.2 Collection Trigger Matrix

| Trigger Event | Learning Type | Priority | Final Promotion Target |
|--------------|---------------|----------|----------------------|
| Automation operation failure | site_interaction | high | TOOLS.md |
| Automation retry success | site_interaction | high | TOOLS.md |
| Selector invalid | site_interaction | high | TOOLS.md |
| SPA navigation anomaly | site_interaction | high | TOOLS.md |
| MCP tool call failure | tool_optimization | high | TOOLS.md |
| MCP tool call timeout | tool_optimization | medium | TOOLS.md |
| User corrects agent output | user_preference | critical | SOUL.md / USER.md |
| User repeated operation pattern | user_preference | medium | USER.md |
| User says "wrong"/"incorrect" | user_preference | critical | SOUL.md |
| User says "I wish you could..." | user_preference | medium | USER.md / AGENTS.md |
| Same-domain success rate < 70% | site_interaction | critical | TOOLS.md |
| Tool token consumption anomaly | tool_optimization | medium | TOOLS.md |
| Abnormal task completion time | site_interaction | medium | AGENTS.md |

**Rate limiting**: Max 5 entries per domain per trigger type per hour to prevent flood from repeated failures.

### 8.3 Deduplication (Heuristics Only)

```rust
fn should_merge(new: &LearningEntry, existing: &[LearningEntry]) -> Option<String> {
    for entry in existing {
        // Exact match on key fields
        if entry.context.domain == new.context.domain
            && entry.context.selector == new.context.selector
            && entry.category == new.category
        {
            return Some(entry.id.clone());
        }

        // Fuzzy match on tool name + summary (Jaccard similarity)
        if entry.context.tool_name == new.context.tool_name
            && jaccard_similarity(&entry.content.summary, &new.content.summary) > 0.85
        {
            return Some(entry.id.clone());
        }
    }
    None
}
```

No LLM-based semantic similarity — heuristics are fast and sufficient for structured entries. Can be added later if needed.

---

## 9. Knowledge Validation and Promotion Pipeline

### 9.1 Validation Flow

```
DashMap (pending)               SQLite (validated)              Five Documents (promoted)
┌──────────────┐             ┌─────────────┐                ┌─────────────┐
│  pending     │             │             │                │             │
│  entries     │── validate ──│  knowledge  │── promote ──→ │ IDENTITY.md │
│              │             │  table      │                │ SOUL.md     │
│ occurrence≥3 │             │             │                │ USER.md     │
│ alive≥24h    │  conf≥0.6  │             │  category-     │ TOOLS.md    │
│ no conflicts │             │             │  specific      │ AGENTS.md   │
│              │             │             │  thresholds    │             │
└──────────────┘             └─────────────┘                └─────────────┘
```

### 9.2 Configurable Thresholds (config.toml)

```toml
[learning.validation]
# Minimum time entry must exist before validation
min_alive_hours = 24
# Minimum occurrence count
min_occurrences = 3
# Minimum confidence score
min_confidence = 0.6

[learning.promotion.site_interaction]
min_hits = 10
min_effectiveness = 0.6
min_alive_days = 7

[learning.promotion.tool_optimization]
min_hits = 10
min_effectiveness = 0.7
min_alive_days = 7

[learning.promotion.user_preference]
min_hits = 5
min_effectiveness = 0.5
min_alive_days = 14
```

### 9.3 Semi-Automatic Promotion Flow

```rust
async fn promote_to_document(knowledge: &Knowledge, soul_manager: &mut SoulManager) -> Result<()> {
    // 1. Determine target document and section
    let (target, section) = route_knowledge(knowledge);

    // 2. Check for conflicts with existing MD content
    if let Some(conflict) = detect_conflict(knowledge, soul_manager, &target, &section)? {
        return handle_conflict(conflict, knowledge, soul_manager).await;
    }

    // 3. Check for conflict with manual edits
    if is_manual_edit_section(soul_manager, &target, &section) {
        log::info!("Skipping promotion: section has manual edits (manual always wins)");
        append_changelog_conflict(knowledge, "manual_edit_priority").await?;
        return Ok(());
    }

    // 4. Distill to concise MD content
    let content = distill_to_md(knowledge);

    // 5. Construct change
    let change = SoulChange {
        target_file: target,
        section,
        change_type: "add".into(),
        new_content: content,
        reason: format!("Knowledge {} promoted (hits: {}, effectiveness: {:.0}%)",
            knowledge.id, knowledge.hit_count, knowledge.effectiveness * 100.0),
        source_knowledge_ids: vec![knowledge.id.clone()],
        confidence: knowledge.confidence,
        source_type: "system".into(),
    };

    // 6. Handle by protection level
    match check_permission(&change.target_file, &change.section) {
        Forbidden => log::warn!("Safety boundaries cannot be modified"),
        RequireDoubleConfirm => {
            queue_pending(change).await;
            notify_critical("Suggested modification to core identity config, please review carefully");
        }
        RequireConfirm => {
            queue_pending(change).await;
            notify_dialog("Suggested config update, confirm?");
        }
        AutoWithNotify => {
            soul_manager.apply_change(change).await?;
            notify_toast("Config auto-updated");
        }
    }

    Ok(())
}

/// Promotion is idempotent: if knowledge already exists in MD, update rather than duplicate
fn is_already_promoted(knowledge: &Knowledge, soul_manager: &SoulManager) -> bool {
    knowledge.source_ids.iter().any(|id|
        soul_manager.has_source_reference(&knowledge.promotion_target, id)
    )
}
```

---

## 10. Knowledge Decay Mechanism

### 10.1 Decay Formula (Lazy Calculation)

Decay is calculated on read, not via batch job. The `last_hit_at` timestamp is stored; decay score is computed at query time.

```rust
fn calculate_decay(last_hit_at: DateTime<Utc>, category: &str,
                   effectiveness: f64, hit_count: u32) -> f64 {
    let days_since_hit = (Utc::now() - last_hit_at).num_days() as f64;

    let base_halflife = match category {
        "site_interaction" => 30.0,     // Websites change often
        "tool_optimization" => 90.0,
        "user_preference" => 180.0,
        _ => 60.0,
    };

    let adjusted_halflife = base_halflife
        * (1.0 + effectiveness.min(1.0))
        * (1.0 + (hit_count as f64).ln() / 10.0);

    (-0.693 * days_since_hit / adjusted_halflife).exp().clamp(0.0, 1.0)
}
```

### 10.2 Decay States

| Decay Score | State | Handling |
|------------|-------|---------|
| 1.0 - 0.5 | active | Normal use |
| 0.5 - 0.2 | decaying | Lower priority in retrieval |
| 0.2 - 0.05 | near_archive | Pending archive; hit can resurrect |
| < 0.05 | archived | Removed from active queries |

Optional weekly cleanup job archives entries with `decay_score < 0.05`.

### 10.3 Resurrection Mechanism

When an archived entry is hit again:

```rust
async fn resurrect_knowledge(knowledge_id: &str) -> Result<()> {
    update_knowledge(knowledge_id, |k| {
        k.status = "validated".into();
        k.last_hit_at = Utc::now();
        k.hit_count += 1;
        // Decay score will be recalculated on next read (lazy)
    }).await?;

    append_changelog_resurrection(knowledge_id).await?;
    Ok(())
}
```

### 10.4 Five-Document Decay Handling

MD file entries don't decay directly (maintains file stability). When all underlying knowledge for a document entry has decayed to `archived`:

1. Mark entry as "possibly outdated" in changelog
2. Notify user to review
3. **Do NOT auto-delete** — prevents accidentally removing user-added content
4. In TOOLS.md site adaptation entries, update "Last verified" to "Expired"

---

## 11. Conflict Resolution

### 11.1 Conflict Types and Handling

| Conflict Type | Description | Resolution |
|--------------|-------------|------------|
| DirectContradiction | Same selector, different conclusions | New overwrites old. **Safeguard**: if old `confidence * hit_count > new * 2`, flag for user arbitration |
| StrategyConflict | Same scenario, different approaches | Keep both, rank by effectiveness. **Cap at 3** strategies per scenario; lowest drops off |
| TemporalConflict | Old knowledge possibly outdated | New overwrites old, old decay accelerated |
| ScopeConflict | General vs specific | Specific takes priority, general gets exception |

### 11.2 Manual Edit Priority

**Manual edits always win over system promotions.** Implementation:

- Track `source_type: manual | system` for each MD section
- When user manually edits a section, mark it as `source_type = manual`
- System promotions that contradict a manual-edit section are rejected
- Rejection is logged in changelog with the conflicting knowledge ID
- User can re-enable auto-updates for a section by removing the manual marker

### 11.3 Five-Document Level Conflict Resolution

When new promotion content contradicts existing MD content:

- Confidence > 0.95 → Suggest replacement, requires user confirmation
- Confidence ≤ 0.95 → Keep existing, record conflict in changelog, notify user

---

## 12. Privacy Tiering

### 12.1 Privacy Levels

| Level | Description | Example |
|-------|-------------|---------|
| public | General tech knowledge, exportable | "React SPA needs hydration wait" |
| internal | Contains domain names, local storage only | "taobao.com uses .item-card" |
| sensitive | Contains user behavior patterns, encrypted at rest | "User often visits finance sites at night" |
| private | Contains personal info, never persisted | Usernames, passwords, form content |

### 12.2 Privacy by Document

| Document | Privacy Level | Exportable | Notes |
|----------|--------------|-----------|-------|
| IDENTITY.md | public | Yes | No user info |
| SOUL.md | public | Yes | No user info |
| USER.md | sensitive | No | Contains user behavior patterns (de-identified) |
| TOOLS.md | internal | Partial | Site adaptations contain domains; tool config exportable |
| AGENTS.md | internal | Partial | Execution strategies exportable; session config not |
| .changelog/ | internal | No | May contain sensitive changes |
| .snapshots/ | sensitive | No | Contains USER.md snapshots |

### 12.3 Encryption

- **Mechanism**: OS keychain (macOS Keychain / Linux Secret Service / Windows Credential Manager) stores AES-256-GCM encryption key
- **Scope**: Sensitive SQLite rows and USER.md file encrypted at rest
- **Key rotation**: Not required for v1; can be added later

### 12.4 Privacy Enforcement

- **Private data**: Filtered at Collector level — never reaches DashMap buffer
- **Export**: `public` = direct export; `internal` = anonymized export (domains → SHA-256 hashes); `sensitive`/`private` = never exported

### 12.5 User Controls

- Directly edit any MD file
- View changelog evolution history
- Rollback to any snapshot
- One-click clear all learning data (DashMap + SQLite + soul/ directory)
- Pause/resume learning collection
- Set domain blacklist (USER.md > Sensitive Domain Blacklist)
- Export public-level content

---

## 13. Runtime Knowledge Retrieval

### 13.1 KnowledgeRetriever

A distinct component (not inline in Agent Runner) with session-scoped caching:

```rust
struct KnowledgeRetriever {
    soul_cache: Arc<FiveDocCache>,  // Loaded at session start, invalidated by FileWatcher
    db: SqlitePool,
}

impl KnowledgeRetriever {
    /// Called at session start — loads five-document cache
    async fn init(&mut self, soul_manager: &SoulManager) -> Result<()> {
        self.soul_cache = soul_manager.cache.clone();
        Ok(())
    }

    /// Called when SoulManager detects file change
    fn invalidate_cache(&mut self, soul_manager: &SoulManager) {
        self.soul_cache = soul_manager.cache.clone();
    }

    /// Main retrieval API — called by Agent Runner before each action
    async fn retrieve_context(&self, action: &ActionContext) -> AgentContext {
        // 1. Five-document cache (session-scoped, already loaded)
        let identity = &self.soul_cache.identity;
        let soul = &self.soul_cache.soul;
        let user = &self.soul_cache.user;
        let tools = &self.soul_cache.tools;
        let agents = &self.soul_cache.agents;

        // 2. Site-specific knowledge (TOOLS.md cache + SQLite fallback)
        let site_knowledge = tools.get_site_adaptation(&action.domain)
            .or_else(|| self.query_site_adaptations(&action.domain).await);

        // 3. Fine-grained knowledge from SQLite (top-K, configurable)
        let relevant_knowledge = self.query_knowledge(
            &action.domain,
            &action.category,
            self.config.top_k,  // default 5, configurable per category
        ).await;

        // 4. Update hit counts (async, fire-and-forget to DashMap buffer)
        for k in &relevant_knowledge {
            self.record_hit_async(&k.id);
        }

        AgentContext {
            identity, soul, user, tools, agents,
            site_knowledge,
            relevant_knowledge,
        }
    }
}
```

### 13.2 Relevance Scoring

Results are ranked by composite relevance score:

```rust
fn relevance_score(entry: &Knowledge, query: &ActionContext) -> f64 {
    let category_match = if entry.category == query.category { 1.0 } else { 0.3 };
    let domain_match = if entry.domain.as_deref() == Some(&query.domain) { 1.0 }
                       else if entry.domain.is_none() { 0.5 }  // universal knowledge
                       else { 0.1 };
    let decay = calculate_decay(entry.last_hit_at, &entry.category,
                                entry.effectiveness, entry.hit_count);
    let confidence = entry.confidence;

    category_match * domain_match * decay * confidence
}
```

---

## 14. Comparison Summary

| Dimension | OpenClaw | NevoFlux v2.0 |
|-----------|---------|---------------|
| Personality system | Single SOUL.md | **Five-document system (identity/soul/user/tools/agents)** |
| Design philosophy | Coding error logs | **Four dimensions: identity + relationship + adaptation + operations** |
| Learning storage | Markdown grep | **DashMap + SQLite (unified)** |
| Document evolution | Manual maintenance | **Semi-automatic + per-section protection levels** |
| Document editing | Direct edit | **Direct edit + system write (compatible, manual always wins)** |
| Retrieval | grep text search | **Structured query + five-doc session cache + relevance scoring** |
| Validation | Human judgment | **Quantitative metrics, configurable thresholds** |
| Decay | None | **Lazy exponential decay + category-specific half-lives + resurrection** |
| Conflict | None | **4 conflict types + auto-resolution + manual-edit priority** |
| Privacy | None | **4-level privacy + OS keychain encryption + anonymized export** |
| Rollback | None | **Snapshots + changelog audit trail** |
| Learning sources | Coding errors/corrections | **Browser interaction full pipeline (3 dimensions)** |
| Multi-session | Basic sessions | **Main/sub-session permission model** |
| Measurability | None | **learning_metrics table + effectiveness tracking** |

---

## 15. Implementation Path

### Phase 0: Instrumentation (1 week)

**Goal**: Define core types and instrument perception points without storage.

- Define `LearningEntry` struct, `LearningSource` trait, `LearningCollector` skeleton
- Instrument Agent Runner (`crates/daemon/src/agent/`), MCP Tools (`crates/mcp/src/tools.rs`), WASM Host (`crates/daemon/src/wasm/`)
- Log-only output (no storage backend) — validate that events are being captured correctly
- **Tests**: Unit tests for entry creation, classification, and LearningSource trait implementations

### Phase 1: Five-Document Framework (2-3 weeks)

**Goal**: Build SoulManager with full five-document lifecycle.

- Create `soul/` directory structure with 5 initial MD files
- Implement `SoulManager`:
  - MD parsing: hybrid (`pulldown-cmark` AST + custom structured data extraction)
  - Async I/O: all file operations via `tokio::fs`
  - FileWatcher: `notify` crate with 500ms debounce
  - FiveDocCache with session-scoped invalidation
- Protection level checks (hardcoded Rust match arms)
- Changelog mechanism (daily files, 90-day retention)
- Snapshot mechanism (tar archives, 50 count + 30-day max retention)
- Rollback functionality
- Integration with existing `crates/storage/` SQLite infrastructure
- **Tests**: SoulManager CRUD, protection level enforcement, snapshot/rollback, manual edit compatibility, changelog format

### Phase 2: Learning Pipeline (2-3 weeks)

**Goal**: Build end-to-end learning pipeline from events to validated knowledge.

- SQLite schema: knowledge, site_adaptations, tool_stats, learning_metrics tables
- DashMap `MemoryBuffer` with configurable flush (30s / 20 entries)
- Heuristic dedup (exact key match + Jaccard similarity)
- Per-trigger-type per-domain rate limiting (max 5/hour)
- Validation pipeline (pending → validated) with configurable thresholds in config.toml
- Semi-automatic promotion flow (validated → promoted to MD)
- Idempotent promotion (source_id tracking prevents duplicates)
- Knowledge routing: category → target document/section
- **Tests**: Pipeline end-to-end, dedup accuracy, rate limiting, promotion flow, threshold configuration

### Phase 3: Intelligence Layer (2 weeks)

**Goal**: Build retrieval, decay, and conflict resolution.

- `KnowledgeRetriever` with session-scoped caching and FileWatcher invalidation
- Composite relevance scoring: `category_match * domain_match * decay * confidence`
- Lazy decay calculation (on read, no batch job)
- Resurrection mechanism for archived entries
- Conflict resolution (4 types: DirectContradiction, StrategyConflict, TemporalConflict, ScopeConflict)
- Manual-edit priority enforcement (`source_type` tracking)
- Learning metrics tracking and effectiveness queries
- Integration with Agent Runner (read path via KnowledgeRetriever)
- **Tests**: Decay calculation correctness, resurrection, conflict detection/resolution, retrieval ranking, relevance scoring

### Phase 4: User Experience & Privacy (1-2 weeks)

**Goal**: Add privacy controls, encryption, and user-facing features.

- OS keychain integration (macOS Keychain / Linux Secret Service / Windows Credential Manager)
- AES-256-GCM encryption for sensitive SQLite rows and USER.md
- Privacy filtering at Collector level (private data never buffered)
- Export functionality: public (direct), internal (anonymized with domain hashing)
- Settings UI: five-document viewer/editor
- Changelog browser
- Snapshot list and rollback UI
- Domain blacklist management
- Learning system controls (pause/resume/clear all data)
- **Tests**: Encryption round-trip, privacy filtering at Collector, export correctness, anonymization
