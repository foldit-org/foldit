import {
	controlKind,
	extractEnumValues,
	isGroupOfGroups,
	normalizeType,
	resolveFieldSchema,
	resolveRef,
	unwrapNullableRef,
} from '../../src/components/util/settingsSchema'
import { describe, it, expect } from 'vitest'

// Hand-authored fixture mirroring the shapes schemars emits for the
// settings structs: nullable scalars as type arrays, nullable enums as
// anyOf with a $defs ref, and a two-level nested object.
const root = {
	type: 'object',
	properties: {
		section: {
			type: 'object',
			properties: {
				plain_flag: { type: 'boolean' },
				plain_strength: { type: 'number', minimum: 0, maximum: 1 },
				plain_mode: { $ref: '#/$defs/ToneMode' },
				maybe_flag: { type: ['boolean', 'null'] },
				maybe_strength: { type: ['number', 'null'], minimum: 0, maximum: 1 },
				maybe_count: { type: ['null', 'integer'], minimum: 0, maximum: 10 },
				maybe_mode: {
					anyOf: [{ $ref: '#/$defs/ToneMode' }, { type: 'null' }],
				},
				bonds: {
					type: 'object',
					properties: {
						enabled: { type: 'boolean' },
						sticks: {
							type: 'object',
							properties: {
								radius: { type: 'number', minimum: 0, maximum: 2 },
							},
						},
					},
				},
				// A pure group-of-groups: every child is itself a group, with no
				// leaf controls at this level (mirrors the `bonds` wrapper around
				// `hydrogen_bonds` + `disulfide_bonds`).
				wrapper: {
					type: 'object',
					properties: {
						hydrogen: {
							type: 'object',
							properties: { visible: { type: 'boolean' } },
						},
						disulfide: {
							type: 'object',
							properties: { visible: { type: 'boolean' } },
						},
					},
				},
			},
		},
	},
	$defs: {
		ToneMode: {
			oneOf: [
				{ const: 'aces', description: 'filmic tone curve' },
				{ enum: ['linear', 'none'] },
			],
		},
	},
}

const field = (name: string) => (root.properties.section.properties as any)[name]

describe('normalizeType', () => {
	it('collapses ["X", "null"] to "X"', () => {
		expect(normalizeType({ type: ['boolean', 'null'] }).type).toBe('boolean')
	})

	it('collapses ["null", "X"] (reversed order) to "X"', () => {
		expect(normalizeType({ type: ['null', 'integer'] }).type).toBe('integer')
	})

	it('preserves sibling keys', () => {
		const s = normalizeType({ type: ['number', 'null'], minimum: 0, maximum: 1 })
		expect(s).toEqual({ type: 'number', minimum: 0, maximum: 1 })
	})

	it('passes through plain string types unchanged', () => {
		const s = { type: 'boolean' }
		expect(normalizeType(s)).toBe(s)
	})

	it('passes through arrays with two non-null types unchanged', () => {
		const s = { type: ['string', 'number'] }
		expect(normalizeType(s)).toBe(s)
	})

	it('passes through schemas without a type unchanged', () => {
		const s = { properties: {} }
		expect(normalizeType(s)).toBe(s)
	})
})

describe('unwrapNullableRef', () => {
	it('returns the non-null branch of anyOf: [ref, null]', () => {
		const s = unwrapNullableRef(field('maybe_mode'))
		expect(s.$ref).toBe('#/$defs/ToneMode')
		expect(s.anyOf).toBeUndefined()
	})

	it('keeps wrapper-level sibling keys (title) on the unwrapped branch', () => {
		const s = unwrapNullableRef({
			anyOf: [{ $ref: '#/$defs/ToneMode' }, { type: 'null' }],
			title: 'Tone Mode',
		})
		expect(s.$ref).toBe('#/$defs/ToneMode')
		expect(s.title).toBe('Tone Mode')
	})

	it('unwraps an inline (non-ref) nullable branch', () => {
		const s = unwrapNullableRef({
			anyOf: [{ type: 'number', minimum: 0, maximum: 5 }, { type: 'null' }],
		})
		expect(s.type).toBe('number')
		expect(s.maximum).toBe(5)
	})

	it('passes through anyOf with two non-null branches unchanged', () => {
		const s = { anyOf: [{ type: 'string' }, { type: 'number' }] }
		expect(unwrapNullableRef(s)).toBe(s)
	})

	it('passes through schemas without anyOf unchanged', () => {
		const s = { type: 'boolean' }
		expect(unwrapNullableRef(s)).toBe(s)
	})
})

describe('resolveFieldSchema', () => {
	it('resolves a nullable enum ref to the same schema as the plain ref', () => {
		const nullable = resolveFieldSchema(field('maybe_mode'), root)
		const plain = resolveFieldSchema(field('plain_mode'), root)
		expect(nullable.oneOf).toEqual(plain.oneOf)
	})

	it('extracts enum values through the nullable wrapper', () => {
		const resolved = resolveFieldSchema(field('maybe_mode'), root)
		expect(extractEnumValues(resolved)).toEqual(['aces', 'linear', 'none'])
	})

	it('keeps slider bounds on a nullable number', () => {
		const s = resolveFieldSchema(field('maybe_strength'), root)
		expect(s).toEqual({ type: 'number', minimum: 0, maximum: 1 })
	})
})

describe('resolveRef', () => {
	it('resolves a $defs ref against the root schema', () => {
		const s = resolveRef(field('plain_mode'), root)
		expect(extractEnumValues(s)).toEqual(['aces', 'linear', 'none'])
	})

	it('passes through schemas without a $ref unchanged', () => {
		const s = field('plain_flag')
		expect(resolveRef(s, root)).toBe(s)
	})
})

describe('controlKind', () => {
	it('classifies plain fields as before (checkbox, slider, select)', () => {
		expect(controlKind(field('plain_flag'), root)).toBe('checkbox')
		expect(controlKind(field('plain_strength'), root)).toBe('slider')
		expect(controlKind(field('plain_mode'), root)).toBe('select')
	})

	it('classifies a nullable boolean as a checkbox', () => {
		expect(controlKind(field('maybe_flag'), root)).toBe('checkbox')
	})

	it('classifies a nullable number with min/max as a slider', () => {
		expect(controlKind(field('maybe_strength'), root)).toBe('slider')
	})

	it('classifies a nullable integer with min/max as a slider', () => {
		expect(controlKind(field('maybe_count'), root)).toBe('slider')
	})

	it('classifies a nullable enum (anyOf ref) as a select', () => {
		expect(controlKind(field('maybe_mode'), root)).toBe('select')
	})

	it('classifies an object with properties as a recursive group', () => {
		expect(controlKind(field('bonds'), root)).toBe('group')
	})

	it('recurses: the nested object expands level by level', () => {
		const bonds = resolveFieldSchema(field('bonds'), root)
		expect(controlKind(bonds.properties.enabled, root)).toBe('checkbox')
		expect(controlKind(bonds.properties.sticks, root)).toBe('group')

		const sticks = resolveFieldSchema(bonds.properties.sticks, root)
		expect(controlKind(sticks.properties.radius, root)).toBe('slider')
	})

	it('classifies an unrenderable field as none', () => {
		expect(controlKind({ type: 'string' }, root)).toBe('none')
	})
})

describe('isGroupOfGroups', () => {
	it('is true when every child of a group is itself a group', () => {
		expect(isGroupOfGroups(field('wrapper'), root)).toBe(true)
	})

	it('is false when a group has any leaf child', () => {
		// `bonds` has a `enabled` boolean leaf alongside the `sticks` group.
		expect(isGroupOfGroups(field('bonds'), root)).toBe(false)
	})

	it('is false for a leaf field (no properties)', () => {
		expect(isGroupOfGroups(field('plain_flag'), root)).toBe(false)
	})

	it('is false for an empty object (no children)', () => {
		expect(isGroupOfGroups({ type: 'object', properties: {} }, root)).toBe(false)
	})
})
