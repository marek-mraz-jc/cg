export interface Entity { id: string; type: string }

export interface Source { list(): Promise<Entity[]> }

export class HttpSource implements Source {
  async list(): Promise<Entity[]> {
    const r = await fetch("/api/entities");
    return r.json();
  }
}

export async function createEntity(e: Entity): Promise<void> {
  await fetch("/api/entities", { method: "POST", body: JSON.stringify(e) });
}

export async function loadEntity(id: string): Promise<Entity> {
  const r = await fetch(`/api/entities/${id}`);
  return r.json();
}

export async function loadStats(): Promise<number> {
  const r = await fetch("/api/stats");
  return (await r.json()).count;
}

export async function deleteAll(): Promise<void> {
  await fetch("/api/purge", { method: "DELETE" });
}
