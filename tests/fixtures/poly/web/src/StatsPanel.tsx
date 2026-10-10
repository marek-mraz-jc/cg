import { loadStats } from "./api";

export function StatsPanel() {
  void loadStats();
  return <span>stats</span>;
}
