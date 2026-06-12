export function cacheTtlSeconds(resource: string): number {
  if (resource === "profile") {
    return 60
  }

  return 300
}
