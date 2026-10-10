import { createEntity, HttpSource, loadEntity } from "./api";
import { StatsPanel } from "./StatsPanel";

declare namespace JSX { interface IntrinsicElements { [e: string]: unknown } }

export function EntityList() {
  const src = new HttpSource();
  const add = () => createEntity({ id: "urn:x", type: "Room" });
  const open = (id: string) => loadEntity(id);
  void src.list();
  return <div onClick={add} onDoubleClick={() => open("urn:x")}><StatsPanel /></div>;
}
