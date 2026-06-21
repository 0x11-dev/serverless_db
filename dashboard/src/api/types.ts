export const REQUIRED_PAGE_IDS = [
  "projects",
  "api_keys",
  "auth_users",
  "tables",
  "storage",
  "realtime",
  "system_workers",
] as const;

export type DashboardRole = "admin" | "user";
export type PageId = (typeof REQUIRED_PAGE_IDS)[number];
export type PageAccess = "full" | "limited" | "restricted";
export type ProjectStatus = "healthy" | "degraded" | "rehydrating" | "paused";

export interface NavigationItem {
  id: PageId;
  label: string;
  description: string;
  access: PageAccess;
  adminOnly: boolean;
}

export interface Viewer {
  userId: string;
  email: string;
  role: DashboardRole;
  orgId: string;
}

export interface ProjectRecord {
  id: string;
  name: string;
  ownerId: string;
  status: ProjectStatus;
  region: string;
  engine: string;
  bookmark: string;
  storageUsed: string;
  walLag: string;
  replicas: number;
  lastWrite: string;
}

export interface ApiKeyRecord {
  id: string;
  name: string;
  type: "anon" | "authenticated" | "service_role" | "replica";
  projectId: string;
  scope: string;
  secretPreview: string;
  lastUsed: string;
  rotationDue: string;
}

export interface AuthUserRecord {
  id: string;
  email: string;
  role: string;
  projectId: string;
  status: "active" | "invited" | "blocked";
  providers: string[];
  lastSeen: string;
}

export interface TableRecord {
  name: string;
  projectId: string;
  columns: number;
  rows: string;
  policies: number;
  realtime: boolean;
  lastMutation: string;
}

export interface StorageBucketRecord {
  name: string;
  projectId: string;
  objects: string;
  size: string;
  publicAccess: boolean;
  retention: string;
  lastWrite: string;
}

export interface RealtimeChannelRecord {
  id: string;
  projectId: string;
  source: string;
  subscribers: number;
  lastEventId: number;
  lag: string;
  status: "streaming" | "idle" | "backpressure";
}

export interface WorkerRecord {
  id: string;
  kind: "writer" | "compactor" | "replica" | "outbox";
  projectId: string;
  region: string;
  status: "running" | "waiting" | "degraded";
  queueDepth: number;
  leaseTtl: string;
  lastHeartbeat: string;
}

export interface DashboardSnapshot {
  viewer: Viewer;
  generatedAt: string;
  permissions: Record<PageId, PageAccess>;
  projects: ProjectRecord[];
  apiKeys: ApiKeyRecord[];
  authUsers: AuthUserRecord[];
  tables: TableRecord[];
  storageBuckets: StorageBucketRecord[];
  realtimeChannels: RealtimeChannelRecord[];
  workers: WorkerRecord[];
}

export interface DashboardControlPlaneClient {
  getNavigation(role: DashboardRole): Promise<NavigationItem[]>;
  getDashboardSnapshot(role: DashboardRole): Promise<DashboardSnapshot>;
}
