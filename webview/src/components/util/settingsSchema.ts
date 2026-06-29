/**
 * Pure helpers for interpreting the JSON Schema that schemars emits for
 * the settings structs. No Solid imports; everything here is total
 * (unrecognized shapes pass through unchanged) and unit-testable without
 * rendering components.
 *
 * Shapes handled beyond plain {type: "X"} fields:
 *   - nullable type arrays: {"type": ["boolean", "null"]} (Option<bool>,
 *     Option<f32>, ...) collapse to the single non-null type
 *   - nullable refs: {"anyOf": [{"$ref": ...}, {"type": "null"}]}
 *     (Option<Enum>) unwrap to the non-null branch for resolution
 *   - nested objects: fields whose resolved schema has `properties`
 *     classify as a recursive group
 */

/**
 * Collapse a nullable type array to its single non-null type:
 * {"type": ["number", "null"]} (either order) becomes {"type": "number"}.
 * Any other shape (plain string type, no type, arrays with zero or more
 * than one non-null entry) is returned unchanged.
 */
export function normalizeType(schema: any): any {
	if (schema == null || !Array.isArray(schema.type)) return schema;
	const nonNull = schema.type.filter((t: any) => t !== 'null');
	if (nonNull.length !== 1) return schema;
	return { ...schema, type: nonNull[0] };
}

/**
 * Unwrap a nullable anyOf: given {"anyOf": [{...}, {"type": "null"}]},
 * return the non-null branch (a $ref or an inline schema) merged with the
 * wrapper's sibling keys (title, description, default) so field-level
 * metadata survives. Any other shape (no anyOf, no null branch, more than
 * one non-null branch) is returned unchanged.
 */
export function unwrapNullableRef(schema: any): any {
	if (schema == null || !Array.isArray(schema.anyOf)) return schema;
	const isNullBranch = (entry: any) => entry?.type === 'null';
	const nonNull = schema.anyOf.filter((entry: any) => !isNullBranch(entry));
	if (nonNull.length !== 1 || nonNull.length === schema.anyOf.length) {
		return schema;
	}
	const { anyOf: _, ...siblings } = schema;
	return { ...nonNull[0], ...siblings };
}

/**
 * Resolve a top-level $ref against the root schema ($defs and friends).
 * Sibling keys on the referencing schema override the referenced one.
 */
export function resolveRef(schema: any, root: any): any {
	if (!schema?.$ref) return schema;
	const segments = schema.$ref.replace(/^#\//, '').split('/');
	let resolved = root;
	for (const seg of segments) {
		resolved = resolved?.[seg];
	}
	if (!resolved) return schema;
	const { $ref: _, ...overrides } = schema;
	return { ...resolved, ...overrides };
}

/**
 * Full field-schema resolution: unwrap a nullable anyOf wrapper, resolve
 * a $ref, then collapse a nullable type array. A nullable field resolves
 * to the same schema as its non-nullable form.
 */
export function resolveFieldSchema(schema: any, root: any): any {
	return normalizeType(resolveRef(unwrapNullableRef(schema), root));
}

/**
 * Extract all string enum values from a schema.
 * Handles schemars' mixed oneOf format where some entries use `enum` arrays
 * and others use `const` (depending on whether variants have doc comments).
 */
export function extractEnumValues(schema: any): string[] | null {
	// Simple enum array: { "enum": ["a", "b", "c"] }
	if (Array.isArray(schema.enum)) return schema.enum;

	// oneOf with mixed const/enum entries
	if (Array.isArray(schema.oneOf)) {
		const vals: string[] = [];
		for (const entry of schema.oneOf) {
			if (entry.const != null) {
				vals.push(entry.const);
			} else if (Array.isArray(entry.enum)) {
				vals.push(...entry.enum);
			}
		}
		return vals.length > 0 ? vals : null;
	}

	return null;
}

export type ControlKind = 'slider' | 'checkbox' | 'select' | 'group' | 'none';

/**
 * Decide which control a field schema renders as, after full resolution.
 * 'group' means an object with properties that renders as a nested
 * subsection and recurses; 'none' means the field renders nothing.
 */
export function controlKind(schema: any, root: any): ControlKind {
	const s = resolveFieldSchema(schema, root);
	if (
		(s.type === 'number' || s.type === 'integer') &&
		s.minimum != null &&
		s.maximum != null
	) {
		return 'slider';
	}
	if (s.type === 'boolean') return 'checkbox';
	if (extractEnumValues(s)) return 'select';
	if (s.properties != null) return 'group';
	return 'none';
}

/**
 * True when a group's every property is itself a group (a group-of-groups,
 * e.g. a `bonds` wrapper whose only children are `hydrogen_bonds` and
 * `disulfide_bonds`). Such a wrapper adds a redundant nesting level and no
 * leaf controls of its own, so the renderer drops the wrapper header and
 * promotes the child groups directly. A group with any leaf child returns
 * false (it still renders its own header + leaves).
 */
export function isGroupOfGroups(schema: any, root: any): boolean {
	const s = resolveFieldSchema(schema, root);
	const props = s.properties;
	if (props == null) return false;
	const entries = Object.values(props);
	if (entries.length === 0) return false;
	return entries.every((child) => controlKind(child, root) === 'group');
}
