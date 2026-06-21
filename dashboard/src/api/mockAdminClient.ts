import {
  DashboardControlPlaneClient,
  DashboardRole,
  DashboardSnapshot,
  NavigationItem,
  PageAccess,
  PageId,
  REQUIRED_PAGE_IDS,
} from "./types";

const NAVIGATION_COPY: Record<PageId, Omit<NavigationItem, "id" | "access">> = {
  projects: {
    label: "Projects",
    description: "Project status, bookmarks, regions, and cache posture",
    adminOnly: false,
  },
  api_keys: {
    label: "API keys",
    description: "Anon, authenticated, service, and replica credentials",
    adminOnly: true,
  },
  auth_users: {
    label: "Auth users",
    description: "GoTrue-compatible user and session inventory",
    adminOnly: true,
  },
  tables: {
    label: "Tables",
    description: "Schema, rows, policy count, and realtime flags",
    adminOnly: false,
  },
  storage: {
    label: "Storage buckets",
    description: "Bucket health, size, retention, and public access",
    adminOnly: false,
  },
  realtime: {
    label: "Realtime",
    description: "Outbox streams, subscribers, and delivery lag",
    adminOnly: false,
  },
  system_workers: {
    label: "System / Workers",
    description: "Writer, compactor, replica, and outbox worker state",
    adminOnly: true,
  },
};

const BASE_SNAPSHOT: Omit<DashboardSnapshot, "viewer" | "permissions"> = {
  generatedAt: "2026-06-21T09:30:00+08:00",
  projects: [
    {
      id: "blog-app",
      name: "Blog Platform",
      ownerId: "alice",
      status: "healthy",
      region: "us-east-1",
      engine: "sqlite-wal",
      bookmark: "sdb1-000198",
      storageUsed: "412 MB",
      walLag: "0 commits",
      replicas: 2,
      lastWrite: "38s ago",
    },
    {
      id: "billing-demo",
      name: "Billing Demo",
      ownerId: "bob",
      status: "degraded",
      region: "us-west-2",
      engine: "sqlite-wal",
      bookmark: "sdb1-000087",
      storageUsed: "96 MB",
      walLag: "12 commits",
      replicas: 1,
      lastWrite: "7m ago",
    },
    {
      id: "sandbox-alice",
      name: "Alice Sandbox",
      ownerId: "alice",
      status: "rehydrating",
      region: "ap-southeast-1",
      engine: "sqlite-wal",
      bookmark: "sdb1-000044",
      storageUsed: "28 MB",
      walLag: "warming",
      replicas: 0,
      lastWrite: "1h ago",
    },
  ],
  apiKeys: [
    {
      id: "key-anon-blog",
      name: "blog anon key",
      type: "anon",
      projectId: "blog-app",
      scope: "read PostgREST, Storage downloads",
      secretPreview: "eyJhbGci...anon",
      lastUsed: "2m ago",
      rotationDue: "29d",
    },
    {
      id: "key-service-blog",
      name: "blog service role",
      type: "service_role",
      projectId: "blog-app",
      scope: "admin bypass, schema, policy, storage",
      secretPreview: "eyJhbGci...svc",
      lastUsed: "11m ago",
      rotationDue: "6d",
    },
    {
      id: "key-replica-blog",
      name: "replica forwarding token",
      type: "replica",
      projectId: "blog-app",
      scope: "read replica forwarding",
      secretPreview: "sdb_rep...2fd",
      lastUsed: "44s ago",
      rotationDue: "14d",
    },
  ],
  authUsers: [
    {
      id: "alice",
      email: "alice@example.com",
      role: "authenticated",
      projectId: "blog-app",
      status: "active",
      providers: ["email"],
      lastSeen: "4m ago",
    },
    {
      id: "bob",
      email: "bob@example.com",
      role: "authenticated",
      projectId: "blog-app",
      status: "active",
      providers: ["email", "oauth"],
      lastSeen: "22m ago",
    },
    {
      id: "svc-admin",
      email: "service-role@internal",
      role: "service_role",
      projectId: "blog-app",
      status: "active",
      providers: ["token"],
      lastSeen: "11m ago",
    },
  ],
  tables: [
    {
      name: "posts",
      projectId: "blog-app",
      columns: 7,
      rows: "18.2k",
      policies: 4,
      realtime: true,
      lastMutation: "38s ago",
    },
    {
      name: "comments",
      projectId: "blog-app",
      columns: 5,
      rows: "46.5k",
      policies: 2,
      realtime: true,
      lastMutation: "1m ago",
    },
    {
      name: "tags",
      projectId: "blog-app",
      columns: 2,
      rows: "24",
      policies: 1,
      realtime: false,
      lastMutation: "2d ago",
    },
    {
      name: "invoices",
      projectId: "billing-demo",
      columns: 12,
      rows: "3.4k",
      policies: 5,
      realtime: false,
      lastMutation: "7m ago",
    },
  ],
  storageBuckets: [
    {
      name: "media",
      projectId: "blog-app",
      objects: "1,204",
      size: "318 MB",
      publicAccess: false,
      retention: "manifest source",
      lastWrite: "9m ago",
    },
    {
      name: "avatars",
      projectId: "blog-app",
      objects: "381",
      size: "42 MB",
      publicAccess: true,
      retention: "30d",
      lastWrite: "18m ago",
    },
    {
      name: "snapshots",
      projectId: "billing-demo",
      objects: "87",
      size: "96 MB",
      publicAccess: false,
      retention: "admin only",
      lastWrite: "7m ago",
    },
  ],
  realtimeChannels: [
    {
      id: "posts-feed",
      projectId: "blog-app",
      source: "_sdb_outbox.posts",
      subscribers: 18,
      lastEventId: 9821,
      lag: "0.4s",
      status: "streaming",
    },
    {
      id: "storage-events",
      projectId: "blog-app",
      source: "_sdb_outbox.storage.objects",
      subscribers: 3,
      lastEventId: 884,
      lag: "idle",
      status: "idle",
    },
    {
      id: "billing-events",
      projectId: "billing-demo",
      source: "_sdb_outbox.invoices",
      subscribers: 8,
      lastEventId: 4188,
      lag: "12 commits",
      status: "backpressure",
    },
  ],
  workers: [
    {
      id: "writer-blog-use1",
      kind: "writer",
      projectId: "blog-app",
      region: "us-east-1",
      status: "running",
      queueDepth: 2,
      leaseTtl: "27s",
      lastHeartbeat: "4s ago",
    },
    {
      id: "compactor-blog-use1",
      kind: "compactor",
      projectId: "blog-app",
      region: "us-east-1",
      status: "waiting",
      queueDepth: 0,
      leaseTtl: "n/a",
      lastHeartbeat: "33s ago",
    },
    {
      id: "replica-billing-usw2",
      kind: "replica",
      projectId: "billing-demo",
      region: "us-west-2",
      status: "degraded",
      queueDepth: 12,
      leaseTtl: "n/a",
      lastHeartbeat: "1m ago",
    },
    {
      id: "outbox-blog-use1",
      kind: "outbox",
      projectId: "blog-app",
      region: "us-east-1",
      status: "running",
      queueDepth: 0,
      leaseTtl: "n/a",
      lastHeartbeat: "6s ago",
    },
  ],
};

export function getPageAccess(role: DashboardRole, pageId: PageId): PageAccess {
  if (role === "admin") return "full";
  if (pageId === "api_keys" || pageId === "auth_users") return "limited";
  if (pageId === "system_workers") return "restricted";
  return "full";
}

export function getNavigationForRole(role: DashboardRole): NavigationItem[] {
  return REQUIRED_PAGE_IDS.map((id) => ({
    id,
    ...NAVIGATION_COPY[id],
    access: getPageAccess(role, id),
  }));
}

function permissionsFor(role: DashboardRole): Record<PageId, PageAccess> {
  return REQUIRED_PAGE_IDS.reduce(
    (permissions, id) => ({ ...permissions, [id]: getPageAccess(role, id) }),
    {} as Record<PageId, PageAccess>,
  );
}

function snapshotFor(role: DashboardRole): DashboardSnapshot {
  const ownedProjectIds = new Set(["blog-app", "sandbox-alice"]);
  const projectVisible = (projectId: string) => role === "admin" || ownedProjectIds.has(projectId);

  return {
    ...BASE_SNAPSHOT,
    viewer: {
      userId: role === "admin" ? "root-admin" : "alice",
      email: role === "admin" ? "admin@example.com" : "alice@example.com",
      role,
      orgId: "acme",
    },
    permissions: permissionsFor(role),
    projects: BASE_SNAPSHOT.projects.filter((project) => role === "admin" || ownedProjectIds.has(project.id)),
    apiKeys:
      role === "admin"
        ? BASE_SNAPSHOT.apiKeys
        : BASE_SNAPSHOT.apiKeys
            .filter((key) => projectVisible(key.projectId) && (key.type === "anon" || key.type === "service_role"))
            .map((key) =>
              key.type === "service_role"
                ? {
                    ...key,
                    scope: "admin only",
                    secretPreview: "restricted",
                    lastUsed: "hidden",
                    rotationDue: "hidden",
                  }
                : key,
            ),
    authUsers:
      role === "admin"
        ? BASE_SNAPSHOT.authUsers
        : BASE_SNAPSHOT.authUsers.filter((user) => user.id === "alice" && projectVisible(user.projectId)),
    tables: BASE_SNAPSHOT.tables.filter((table) => projectVisible(table.projectId)),
    storageBuckets: BASE_SNAPSHOT.storageBuckets.filter((bucket) => projectVisible(bucket.projectId)),
    realtimeChannels: BASE_SNAPSHOT.realtimeChannels.filter((channel) => projectVisible(channel.projectId)),
    workers: role === "admin" ? BASE_SNAPSHOT.workers : [],
  };
}

export class MockAdminClient implements DashboardControlPlaneClient {
  async getNavigation(role: DashboardRole): Promise<NavigationItem[]> {
    return getNavigationForRole(role);
  }

  async getDashboardSnapshot(role: DashboardRole): Promise<DashboardSnapshot> {
    return snapshotFor(role);
  }
}
