import { useEffect, useMemo, useState } from "react";
import {
  Activity,
  Circle,
  Database,
  HardDrive,
  KeyRound,
  LockKeyhole,
  LucideIcon,
  Radio,
  RefreshCw,
  Search,
  ServerCog,
  Shield,
  Table2,
  User,
  Users,
} from "lucide-react";
import { MockAdminClient } from "./api/mockAdminClient";
import {
  ApiKeyRecord,
  AuthUserRecord,
  DashboardRole,
  DashboardSnapshot,
  NavigationItem,
  PageAccess,
  PageId,
  ProjectRecord,
  RealtimeChannelRecord,
  StorageBucketRecord,
  TableRecord,
  WorkerRecord,
} from "./api/types";

const client = new MockAdminClient();

const PAGE_ICONS: Record<PageId, LucideIcon> = {
  projects: Database,
  api_keys: KeyRound,
  auth_users: Users,
  tables: Table2,
  storage: HardDrive,
  realtime: Radio,
  system_workers: ServerCog,
};

const ACCESS_LABEL: Record<PageAccess, string> = {
  full: "Full",
  limited: "Limited",
  restricted: "Admin only",
};

export function App() {
  const [role, setRole] = useState<DashboardRole>("admin");
  const [activePage, setActivePage] = useState<PageId>("projects");
  const [navigation, setNavigation] = useState<NavigationItem[]>([]);
  const [snapshot, setSnapshot] = useState<DashboardSnapshot | null>(null);
  const [selectedProjectId, setSelectedProjectId] = useState("blog-app");
  const [query, setQuery] = useState("");

  useEffect(() => {
    let cancelled = false;
    Promise.all([client.getNavigation(role), client.getDashboardSnapshot(role)]).then(([nextNavigation, nextSnapshot]) => {
      if (cancelled) return;
      setNavigation(nextNavigation);
      setSnapshot(nextSnapshot);
      if (!nextSnapshot.projects.some((project) => project.id === selectedProjectId)) {
        setSelectedProjectId(nextSnapshot.projects[0]?.id ?? "");
      }
    });
    return () => {
      cancelled = true;
    };
  }, [role, selectedProjectId]);

  const selectedProject = useMemo(
    () => snapshot?.projects.find((project) => project.id === selectedProjectId) ?? snapshot?.projects[0],
    [snapshot, selectedProjectId],
  );

  const filteredProjects = useMemo(() => {
    if (!snapshot) return [];
    const needle = query.trim().toLowerCase();
    if (!needle) return snapshot.projects;
    return snapshot.projects.filter(
      (project) =>
        project.name.toLowerCase().includes(needle) ||
        project.id.toLowerCase().includes(needle) ||
        project.region.toLowerCase().includes(needle),
    );
  }, [query, snapshot]);

  if (!snapshot) {
    return <div className="loading">Loading dashboard snapshot...</div>;
  }

  const activeAccess = snapshot.permissions[activePage];
  const ownedProjectCount = snapshot.projects.length;
  const activeProjectCount = snapshot.projects.filter((project) => project.status === "healthy").length;
  const warningCount = snapshot.projects.filter((project) => project.status !== "healthy").length;

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand-block">
          <Database size={18} />
          <div>
            <div className="brand-title">Serverless DB</div>
            <div className="brand-subtitle">Control console</div>
          </div>
        </div>

        <nav className="nav-list" aria-label="Dashboard pages">
          {navigation.map((item) => {
            const Icon = PAGE_ICONS[item.id];
            return (
              <button
                className={`nav-item ${item.id === activePage ? "active" : ""}`}
                key={item.id}
                onClick={() => setActivePage(item.id)}
                title={item.description}
                type="button"
              >
                <Icon size={16} />
                <span>{item.label}</span>
                {item.access !== "full" ? (
                  <span className={`nav-access nav-access-${item.access}`}>{ACCESS_LABEL[item.access]}</span>
                ) : null}
              </button>
            );
          })}
        </nav>

        <div className="sidebar-footer">
          <div className="meta-label">Adapter</div>
          <div className="meta-value">Mock control-plane facade</div>
        </div>
      </aside>

      <main className="workspace">
        <header className="topbar">
          <div>
            <div className="eyebrow">Dashboard v2</div>
            <h1>Project operations</h1>
          </div>

          <div className="toolbar">
            <label className="search-box">
              <Search size={15} />
              <input
                aria-label="Search projects"
                onChange={(event) => setQuery(event.target.value)}
                placeholder="Search projects"
                value={query}
              />
            </label>
            <div className="role-switch" aria-label="View role">
              <button
                className={role === "admin" ? "selected" : ""}
                onClick={() => setRole("admin")}
                type="button"
              >
                <Shield size={14} />
                Admin
              </button>
              <button className={role === "user" ? "selected" : ""} onClick={() => setRole("user")} type="button">
                <User size={14} />
                User
              </button>
            </div>
            <button className="icon-button" title="Refresh snapshot" type="button">
              <RefreshCw size={15} />
            </button>
          </div>
        </header>

        <section className="summary-strip" aria-label="Dashboard summary">
          <Metric label="Visible projects" value={ownedProjectCount.toString()} />
          <Metric label="Healthy" value={activeProjectCount.toString()} tone="good" />
          <Metric label="Needs attention" value={warningCount.toString()} tone={warningCount > 0 ? "warn" : "good"} />
          <Metric label="Realtime subscribers" value={sum(snapshot.realtimeChannels.map((channel) => channel.subscribers)).toString()} />
          <Metric label="Generated" value={snapshot.generatedAt.replace("T", " ").slice(0, 16)} />
        </section>

        <section className="content-grid">
          <section className="panel main-panel">
            <PageContent
              access={activeAccess}
              activePage={activePage}
              filteredProjects={filteredProjects}
              onSelectProject={setSelectedProjectId}
              role={role}
              selectedProjectId={selectedProject?.id ?? ""}
              snapshot={snapshot}
            />
          </section>

          <aside className="panel inspector">
            <Inspector
              activeAccess={activeAccess}
              activePage={activePage}
              navigation={navigation}
              role={role}
              selectedProject={selectedProject}
              snapshot={snapshot}
            />
          </aside>
        </section>
      </main>
    </div>
  );
}

function PageContent({
  access,
  activePage,
  filteredProjects,
  onSelectProject,
  role,
  selectedProjectId,
  snapshot,
}: {
  access: PageAccess;
  activePage: PageId;
  filteredProjects: ProjectRecord[];
  onSelectProject: (projectId: string) => void;
  role: DashboardRole;
  selectedProjectId: string;
  snapshot: DashboardSnapshot;
}) {
  if (access === "restricted") {
    return (
      <div className="page-stack">
        <PageTitle access={access} description="Worker internals require an administrator session." title="System / Workers" />
        <LockedState role={role} />
      </div>
    );
  }

  if (activePage === "projects") {
    return (
      <div className="page-stack">
        <PageTitle access={access} description="Project health, durable bookmark, replica count, and storage pressure." title="Projects" />
        <ProjectsTable onSelectProject={onSelectProject} projects={filteredProjects} selectedProjectId={selectedProjectId} />
      </div>
    );
  }

  if (activePage === "api_keys") {
    return (
      <div className="page-stack">
        <PageTitle
          access={access}
          description="Admin sees service credentials. User mode keeps the page skeleton but redacts admin tokens."
          title="API keys"
        />
        <ApiKeysTable keys={snapshot.apiKeys} />
      </div>
    );
  }

  if (activePage === "auth_users") {
    return (
      <div className="page-stack">
        <PageTitle
          access={access}
          description="Admin inventory for GoTrue-compatible users; ordinary users only see their own account row."
          title="Auth users"
        />
        <AuthUsersTable users={snapshot.authUsers} />
      </div>
    );
  }

  if (activePage === "tables") {
    return (
      <div className="page-stack">
        <PageTitle access={access} description="Schema, policy count, and realtime flags for visible projects." title="Tables" />
        <TablesTable tables={snapshot.tables} />
      </div>
    );
  }

  if (activePage === "storage") {
    return (
      <div className="page-stack">
        <PageTitle access={access} description="Bucket visibility, object count, retention, and recent writes." title="Storage buckets" />
        <StorageTable buckets={snapshot.storageBuckets} />
      </div>
    );
  }

  if (activePage === "realtime") {
    return (
      <div className="page-stack">
        <PageTitle access={access} description="Realtime outbox streams, subscribers, event ids, and lag." title="Realtime" />
        <RealtimeTable channels={snapshot.realtimeChannels} />
      </div>
    );
  }

  return (
    <div className="page-stack">
      <PageTitle access={access} description="Worker state, queue depth, and lease heartbeats." title="System / Workers" />
      <WorkersTable workers={snapshot.workers} />
    </div>
  );
}

function PageTitle({ access, description, title }: { access: PageAccess; description: string; title: string }) {
  return (
    <div className="page-title-row">
      <div>
        <h2>{title}</h2>
        <p>{description}</p>
      </div>
      <AccessPill access={access} />
    </div>
  );
}

function ProjectsTable({
  onSelectProject,
  projects,
  selectedProjectId,
}: {
  onSelectProject: (projectId: string) => void;
  projects: ProjectRecord[];
  selectedProjectId: string;
}) {
  return (
    <div className="table-wrap">
      <table>
        <thead>
          <tr>
            <th>Project</th>
            <th>Status</th>
            <th>Region</th>
            <th>Bookmark</th>
            <th>Storage</th>
            <th>Replica lag</th>
            <th>Last write</th>
          </tr>
        </thead>
        <tbody>
          {projects.map((project) => (
            <tr
              className={project.id === selectedProjectId ? "selected-row" : ""}
              key={project.id}
              onClick={() => onSelectProject(project.id)}
            >
              <td>
                <div className="primary-cell">{project.name}</div>
                <div className="secondary-cell">{project.id}</div>
              </td>
              <td>
                <StatusPill status={project.status} />
              </td>
              <td>{project.region}</td>
              <td className="mono">{project.bookmark}</td>
              <td>{project.storageUsed}</td>
              <td>{project.walLag}</td>
              <td>{project.lastWrite}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function ApiKeysTable({ keys }: { keys: ApiKeyRecord[] }) {
  return (
    <div className="table-wrap">
      <table>
        <thead>
          <tr>
            <th>Name</th>
            <th>Type</th>
            <th>Project</th>
            <th>Scope</th>
            <th>Secret</th>
            <th>Last used</th>
            <th>Rotate</th>
          </tr>
        </thead>
        <tbody>
          {keys.map((key) => (
            <tr key={key.id}>
              <td className="primary-cell">{key.name}</td>
              <td>
                <CodePill>{key.type}</CodePill>
              </td>
              <td>{key.projectId}</td>
              <td>{key.scope}</td>
              <td className="mono">{key.secretPreview}</td>
              <td>{key.lastUsed}</td>
              <td>{key.rotationDue}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function AuthUsersTable({ users }: { users: AuthUserRecord[] }) {
  return (
    <div className="table-wrap">
      <table>
        <thead>
          <tr>
            <th>User</th>
            <th>Role</th>
            <th>Project</th>
            <th>Status</th>
            <th>Providers</th>
            <th>Last seen</th>
          </tr>
        </thead>
        <tbody>
          {users.map((user) => (
            <tr key={user.id}>
              <td>
                <div className="primary-cell">{user.email}</div>
                <div className="secondary-cell">{user.id}</div>
              </td>
              <td>{user.role}</td>
              <td>{user.projectId}</td>
              <td>
                <StatusPill status={user.status} />
              </td>
              <td>{user.providers.join(", ")}</td>
              <td>{user.lastSeen}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function TablesTable({ tables }: { tables: TableRecord[] }) {
  return (
    <div className="table-wrap">
      <table>
        <thead>
          <tr>
            <th>Table</th>
            <th>Project</th>
            <th>Columns</th>
            <th>Rows</th>
            <th>Policies</th>
            <th>Realtime</th>
            <th>Last mutation</th>
          </tr>
        </thead>
        <tbody>
          {tables.map((table) => (
            <tr key={`${table.projectId}:${table.name}`}>
              <td className="primary-cell">{table.name}</td>
              <td>{table.projectId}</td>
              <td>{table.columns}</td>
              <td>{table.rows}</td>
              <td>{table.policies}</td>
              <td>{table.realtime ? "enabled" : "off"}</td>
              <td>{table.lastMutation}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function StorageTable({ buckets }: { buckets: StorageBucketRecord[] }) {
  return (
    <div className="table-wrap">
      <table>
        <thead>
          <tr>
            <th>Bucket</th>
            <th>Project</th>
            <th>Objects</th>
            <th>Size</th>
            <th>Public</th>
            <th>Retention</th>
            <th>Last write</th>
          </tr>
        </thead>
        <tbody>
          {buckets.map((bucket) => (
            <tr key={`${bucket.projectId}:${bucket.name}`}>
              <td className="primary-cell">{bucket.name}</td>
              <td>{bucket.projectId}</td>
              <td>{bucket.objects}</td>
              <td>{bucket.size}</td>
              <td>{bucket.publicAccess ? "yes" : "no"}</td>
              <td>{bucket.retention}</td>
              <td>{bucket.lastWrite}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function RealtimeTable({ channels }: { channels: RealtimeChannelRecord[] }) {
  return (
    <div className="table-wrap">
      <table>
        <thead>
          <tr>
            <th>Channel</th>
            <th>Project</th>
            <th>Source</th>
            <th>Subscribers</th>
            <th>Last event</th>
            <th>Lag</th>
            <th>Status</th>
          </tr>
        </thead>
        <tbody>
          {channels.map((channel) => (
            <tr key={channel.id}>
              <td className="primary-cell">{channel.id}</td>
              <td>{channel.projectId}</td>
              <td className="mono">{channel.source}</td>
              <td>{channel.subscribers}</td>
              <td>{channel.lastEventId}</td>
              <td>{channel.lag}</td>
              <td>
                <StatusPill status={channel.status} />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function WorkersTable({ workers }: { workers: WorkerRecord[] }) {
  return (
    <div className="table-wrap">
      <table>
        <thead>
          <tr>
            <th>Worker</th>
            <th>Kind</th>
            <th>Project</th>
            <th>Region</th>
            <th>Status</th>
            <th>Queue</th>
            <th>Lease TTL</th>
            <th>Heartbeat</th>
          </tr>
        </thead>
        <tbody>
          {workers.map((worker) => (
            <tr key={worker.id}>
              <td className="primary-cell">{worker.id}</td>
              <td>{worker.kind}</td>
              <td>{worker.projectId}</td>
              <td>{worker.region}</td>
              <td>
                <StatusPill status={worker.status} />
              </td>
              <td>{worker.queueDepth}</td>
              <td>{worker.leaseTtl}</td>
              <td>{worker.lastHeartbeat}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function Inspector({
  activeAccess,
  activePage,
  navigation,
  role,
  selectedProject,
  snapshot,
}: {
  activeAccess: PageAccess;
  activePage: PageId;
  navigation: NavigationItem[];
  role: DashboardRole;
  selectedProject?: ProjectRecord;
  snapshot: DashboardSnapshot;
}) {
  return (
    <div className="inspector-stack">
      <div className="inspector-section">
        <div className="section-heading">Selected project</div>
        {selectedProject ? (
          <dl className="detail-list">
            <div>
              <dt>Name</dt>
              <dd>{selectedProject.name}</dd>
            </div>
            <div>
              <dt>Engine</dt>
              <dd>{selectedProject.engine}</dd>
            </div>
            <div>
              <dt>Bookmark</dt>
              <dd className="mono">{selectedProject.bookmark}</dd>
            </div>
            <div>
              <dt>Replicas</dt>
              <dd>{selectedProject.replicas}</dd>
            </div>
          </dl>
        ) : (
          <div className="empty-state">No visible project.</div>
        )}
      </div>

      <div className="inspector-section">
        <div className="section-heading">Role boundary</div>
        <div className="role-card">
          <div className="role-card-icon">{role === "admin" ? <Shield size={16} /> : <User size={16} />}</div>
          <div>
            <div className="primary-cell">{snapshot.viewer.email}</div>
            <div className="secondary-cell">{role === "admin" ? "Administrator view" : "Ordinary user view"}</div>
          </div>
        </div>
        <div className="permission-list">
          {navigation.map((item) => {
            const Icon = PAGE_ICONS[item.id];
            return (
              <div className="permission-row" key={item.id}>
                <Icon size={14} />
                <span>{item.label}</span>
                <AccessPill access={item.access} compact />
              </div>
            );
          })}
        </div>
      </div>

      <div className="inspector-section">
        <div className="section-heading">Facade contract</div>
        <div className="contract-list">
          <div>
            <Circle size={8} />
            <span>UI calls typed control-plane client only.</span>
          </div>
          <div>
            <Circle size={8} />
            <span>Mock data mirrors data-plane concepts without touching Rust routes.</span>
          </div>
          <div>
            <Circle size={8} />
            <span>
              Active page access: <strong>{ACCESS_LABEL[activeAccess]}</strong> on <strong>{activePage}</strong>.
            </span>
          </div>
        </div>
      </div>
    </div>
  );
}

function LockedState({ role }: { role: DashboardRole }) {
  return (
    <div className="locked-state">
      <LockKeyhole size={18} />
      <div>
        <div className="primary-cell">Worker details are hidden in {role} mode.</div>
        <div className="secondary-cell">The page remains in navigation so access rules can be verified before real admin APIs exist.</div>
      </div>
    </div>
  );
}

function Metric({ label, tone = "neutral", value }: { label: string; tone?: "neutral" | "good" | "warn"; value: string }) {
  return (
    <div className={`metric metric-${tone}`}>
      <span>{label}</span>
      <strong>{value}</strong>
    </div>
  );
}

function AccessPill({ access, compact = false }: { access: PageAccess; compact?: boolean }) {
  return <span className={`access-pill access-${access} ${compact ? "compact" : ""}`}>{ACCESS_LABEL[access]}</span>;
}

function StatusPill({ status }: { status: string }) {
  const normalized = status.replaceAll("_", "-");
  return (
    <span className={`status-pill status-${normalized}`}>
      <Activity size={11} />
      {status}
    </span>
  );
}

function CodePill({ children }: { children: string }) {
  return <span className="code-pill">{children}</span>;
}

function sum(values: number[]) {
  return values.reduce((total, value) => total + value, 0);
}
