/**
 * Kubernetes Manifest Builder for StellarNode Topology Spread Constraints.
 *
 * Converts visual node/zone topology configuration into standard Kubernetes YAML manifests
 * adhering strictly to the `stellar.org/v1alpha1` StellarNode CustomResourceDefinition.
 */

export interface NodeConfig {
  id: string;
  name: string;
  nodeType: 'Validator' | 'Horizon' | 'SorobanRpc';
  zone: string;
  network?: 'mainnet' | 'testnet' | 'futurenet' | 'custom';
  version?: string;
}

export interface TopologySpreadSettings {
  maxSkew: number;
  topologyKey: string;
  whenUnsatisfiable: 'DoNotSchedule' | 'ScheduleAnyway';
  matchLabelKey?: string;
  matchLabelValue?: string;
}

export interface ZoneConfig {
  id: string;
  name: string;
  region?: string;
}

export interface TopologyBuilderState {
  zones: ZoneConfig[];
  nodes: NodeConfig[];
  spreadSettings: TopologySpreadSettings;
  namespace?: string;
}

export interface TopologySpreadConstraintYAML {
  maxSkew: number;
  topologyKey: string;
  whenUnsatisfiable: 'DoNotSchedule' | 'ScheduleAnyway';
  labelSelector: {
    matchLabels: Record<string, string>;
  };
}

export interface StellarNodeManifestObject {
  apiVersion: string;
  kind: string;
  metadata: {
    name: string;
    namespace: string;
    labels: Record<string, string>;
  };
  spec: {
    nodeType: string;
    network: string;
    version: string;
    replicas: number;
    podAntiAffinity: string;
    topologySpreadConstraints: TopologySpreadConstraintYAML[];
    storage?: {
      mode: string;
      size: string;
      storageClass: string;
    };
  };
}

/**
 * Builds standard Kubernetes TopologySpreadConstraint specs array.
 */
export function buildTopologySpreadConstraints(
  settings: TopologySpreadSettings
): TopologySpreadConstraintYAML[] {
  const matchLabelKey = settings.matchLabelKey || 'app.kubernetes.io/name';
  const matchLabelValue = settings.matchLabelValue || 'stellarnode';

  return [
    {
      maxSkew: settings.maxSkew ?? 1,
      topologyKey: settings.topologyKey || 'topology.kubernetes.io/zone',
      whenUnsatisfiable: settings.whenUnsatisfiable || 'DoNotSchedule',
      labelSelector: {
        matchLabels: {
          [matchLabelKey]: matchLabelValue,
        },
      },
    },
  ];
}

/**
 * Builds a single StellarNode Custom Resource manifest object.
 */
export function buildStellarNodeManifestObject(
  node: NodeConfig,
  spreadSettings: TopologySpreadSettings,
  namespace = 'stellar'
): StellarNodeManifestObject {
  const safeName = (node.name || `node-${node.id}`)
    .toLowerCase()
    .replace(/[^a-z0-9-]/g, '-');

  return {
    apiVersion: 'stellar.org/v1alpha1',
    kind: 'StellarNode',
    metadata: {
      name: safeName,
      namespace: namespace || 'stellar',
      labels: {
        'app.kubernetes.io/name': 'stellarnode',
        'app.kubernetes.io/component': node.nodeType.toLowerCase(),
        'topology.kubernetes.io/zone': node.zone,
      },
    },
    spec: {
      nodeType: node.nodeType || 'Validator',
      network: node.network || 'mainnet',
      version: node.version || 'v21.0.0',
      replicas: 1,
      podAntiAffinity: 'Hard',
      topologySpreadConstraints: buildTopologySpreadConstraints(spreadSettings),
      storage: {
        mode: 'PersistentVolume',
        size: node.nodeType === 'Validator' ? '500Gi' : '100Gi',
        storageClass: 'ssd-premium',
      },
    },
  };
}

/**
 * Simple, clean YAML serializer helper formatted without external dependencies.
 */
export function dumpYaml(obj: any, indentLevel = 0): string {
  const spaces = ' '.repeat(indentLevel);

  if (obj === null || obj === undefined) {
    return 'null';
  }

  if (typeof obj === 'boolean' || typeof obj === 'number') {
    return String(obj);
  }

  if (typeof obj === 'string') {
    // If string contains special characters or quotes, wrap appropriately
    if (obj.includes('\n')) {
      return `|\n` + obj.split('\n').map(line => spaces + '  ' + line).join('\n');
    }
    if (/[:#\[\]\{\},&\*!|\>'"%@`]/.test(obj) || obj.trim() !== obj) {
      return `"${obj.replace(/"/g, '\\"')}"`;
    }
    return obj;
  }

  if (Array.isArray(obj)) {
    if (obj.length === 0) return '[]';
    return obj
      .map(item => {
        if (typeof item === 'object' && item !== null) {
          const itemYaml = dumpYaml(item, indentLevel + 2);
          const lines = itemYaml.split('\n');
          const firstLine = `${spaces}- ${lines[0].trimStart()}`;
          const remainingLines = lines.slice(1).map(l => l);
          return [firstLine, ...remainingLines].join('\n');
        } else {
          return `${spaces}- ${dumpYaml(item, 0)}`;
        }
      })
      .join('\n');
  }

  if (typeof obj === 'object') {
    const keys = Object.keys(obj);
    if (keys.length === 0) return '{}';
    return keys
      .map(key => {
        const val = obj[key];
        if (typeof val === 'object' && val !== null) {
          if (Array.isArray(val) && val.length === 0) {
            return `${spaces}${key}: []`;
          }
          if (!Array.isArray(val) && Object.keys(val).length === 0) {
            return `${spaces}${key}: {}`;
          }
          return `${spaces}${key}:\n${dumpYaml(val, indentLevel + 2)}`;
        }
        return `${spaces}${key}: ${dumpYaml(val, 0)}`;
      })
      .join('\n');
  }

  return String(obj);
}

/**
 * Generates Kubernetes YAML manifest string for the full topology configuration.
 */
export function generateTopologyManifestYaml(state: TopologyBuilderState): string {
  if (!state.nodes || state.nodes.length === 0) {
    return '# No nodes placed in topology configuration.\n';
  }

  const namespace = state.namespace || 'stellar';
  const manifestObjects = state.nodes.map(node =>
    buildStellarNodeManifestObject(node, state.spreadSettings, namespace)
  );

  if (manifestObjects.length === 1) {
    return dumpYaml(manifestObjects[0]) + '\n';
  }

  // Multi-resource output separating documents with standard YAML document separator '---'
  return manifestObjects.map(obj => dumpYaml(obj)).join('\n---\n') + '\n';
}
