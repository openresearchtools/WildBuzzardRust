use crate::geometry::Au;
use crate::style::{AlignItems, FlexFactor, FlexWrap, JustifyContent};

#[derive(Clone, Copy, Debug)]
pub(crate) struct FlexItemInput {
    pub source_index: usize,
    pub order: i32,
    pub base_main: Au,
    pub min_main: Au,
    pub max_main: Option<Au>,
    pub grow: FlexFactor,
    pub shrink: FlexFactor,
    pub outer_main: Au,
    pub base_cross: Au,
    pub min_cross: Au,
    pub max_cross: Option<Au>,
    pub outer_cross: Au,
    pub cross_auto: bool,
    pub align: AlignItems,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FlexConstraints {
    pub main_size: Option<Au>,
    pub cross_size: Option<Au>,
    pub wrap: FlexWrap,
    pub main_gap: Au,
    pub cross_gap: Au,
    pub justify_content: JustifyContent,
    pub max_lines: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FlexPlacement {
    pub source_index: usize,
    pub target_main: Au,
    pub target_cross: Au,
    /// Offset of the item's outer margin edge from the container's main start.
    pub outer_main_offset: Au,
    /// Offset of the item's outer margin edge from the container's cross start.
    pub outer_cross_offset: Au,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FlexPlan {
    pub placements: Vec<FlexPlacement>,
    pub main_extent: Au,
    pub cross_extent: Au,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FlexError {
    WorkLimitExceeded {
        limit: usize,
    },
    LineLimitExceeded {
        limit: usize,
    },
    AllocationFailed {
        resource: &'static str,
        requested: usize,
    },
    ArithmeticOverflow,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FlexWorkBudget {
    limit: usize,
    used: usize,
}

impl FlexWorkBudget {
    pub(crate) const fn new(limit: usize) -> Self {
        Self { limit, used: 0 }
    }

    /// Charges a complete pass before the pass mutates planner state.
    pub(crate) fn charge(&mut self, amount: usize) -> Result<(), FlexError> {
        let used = self
            .used
            .checked_add(amount)
            .ok_or(FlexError::WorkLimitExceeded { limit: self.limit })?;
        if used > self.limit {
            return Err(FlexError::WorkLimitExceeded { limit: self.limit });
        }
        self.used = used;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct WorkingItem {
    input: FlexItemInput,
    hypothetical: i128,
    target: i128,
    frozen: bool,
}

#[derive(Clone, Copy, Debug)]
struct FlexLine {
    start: usize,
    end: usize,
    main_size: i128,
    cross_size: i128,
    cross_offset: i128,
}

pub(crate) fn plan_flex_layout(
    inputs: &[FlexItemInput],
    constraints: FlexConstraints,
    budget: &mut FlexWorkBudget,
) -> Result<FlexPlan, FlexError> {
    let item_count = inputs.len();
    budget.charge(item_count)?;
    let mut items = Vec::new();
    items
        .try_reserve_exact(item_count)
        .map_err(|_| FlexError::AllocationFailed {
            resource: "flex items",
            requested: item_count,
        })?;
    for input in inputs {
        items.push(WorkingItem {
            input: *input,
            hypothetical: clamp_main(
                raw(input.base_main),
                raw(input.min_main),
                input.max_main.map(raw),
            ),
            target: raw(input.base_main),
            frozen: false,
        });
    }

    // The DOM index makes this key unique, so an allocation-free unstable sort
    // still has stable CSS `order` behavior for equal order values.
    budget.charge(sort_work(item_count)?)?;
    items.sort_unstable_by_key(|item| (item.input.order, item.input.source_index));

    budget.charge(item_count)?;
    let mut lines = Vec::new();
    let line_capacity = required_line_capacity(item_count, constraints.max_lines);
    lines
        .try_reserve_exact(line_capacity)
        .map_err(|_| FlexError::AllocationFailed {
            resource: "flex lines",
            requested: line_capacity,
        })?;
    build_lines(&items, constraints, &mut lines)?;

    budget.charge(lines.len())?;
    for line in &mut lines {
        resolve_flexible_lengths(&mut items[line.start..line.end], line, constraints, budget)?;
    }

    resolve_cross_sizes(&items, &mut lines, constraints, budget)?;

    // Each line performs one read-only used-size pass followed by one
    // fragment-placement pass. Charge both over the complete item set before
    // the first placement is published into the plan.
    budget.charge(item_count)?;
    budget.charge(item_count)?;
    let mut placements = Vec::new();
    placements
        .try_reserve_exact(item_count)
        .map_err(|_| FlexError::AllocationFailed {
            resource: "flex placements",
            requested: item_count,
        })?;
    for line in &lines {
        append_line_placements(
            &items[line.start..line.end],
            *line,
            constraints,
            &mut placements,
        )?;
    }

    budget.charge(lines.len())?;
    let main_extent = lines.iter().map(|line| line.main_size).max().unwrap_or(0);
    let cross_extent = if let Some(cross_size) = constraints.cross_size {
        raw(cross_size)
    } else {
        budget.charge(lines.len())?;
        extent_with_gaps(
            lines.iter().map(|line| line.cross_size),
            raw(constraints.cross_gap),
        )?
    };
    Ok(FlexPlan {
        placements,
        main_extent: checked_au(main_extent)?,
        cross_extent: checked_au(cross_extent)?,
    })
}

fn required_line_capacity(item_count: usize, max_lines: usize) -> usize {
    item_count.max(1).min(max_lines)
}

fn build_lines(
    items: &[WorkingItem],
    constraints: FlexConstraints,
    lines: &mut Vec<FlexLine>,
) -> Result<(), FlexError> {
    if items.is_empty() {
        push_line(lines, 0, 0, constraints.max_lines)?;
        return Ok(());
    }
    let available = constraints.main_size.map(raw);
    let gap = raw(constraints.main_gap);
    let mut start = 0;
    let mut occupied = 0_i128;
    for (index, item) in items.iter().enumerate() {
        let outer = checked_add(item.hypothetical, raw(item.input.outer_main))?;
        let with_gap = if index == start {
            outer
        } else {
            checked_add(gap, outer)?
        };
        let wraps = constraints.wrap == FlexWrap::Wrap
            && available.is_some()
            && index > start
            && checked_add(occupied, with_gap)? > available.unwrap_or_default();
        if wraps {
            push_line(lines, start, index, constraints.max_lines)?;
            start = index;
            occupied = outer;
        } else {
            occupied = checked_add(occupied, with_gap)?;
        }
    }
    push_line(lines, start, items.len(), constraints.max_lines)
}

fn push_line(
    lines: &mut Vec<FlexLine>,
    start: usize,
    end: usize,
    limit: usize,
) -> Result<(), FlexError> {
    if lines.len() >= limit {
        return Err(FlexError::LineLimitExceeded { limit });
    }
    lines.push(FlexLine {
        start,
        end,
        main_size: 0,
        cross_size: 0,
        cross_offset: 0,
    });
    Ok(())
}

fn resolve_flexible_lengths(
    items: &mut [WorkingItem],
    line: &mut FlexLine,
    constraints: FlexConstraints,
    budget: &mut FlexWorkBudget,
) -> Result<(), FlexError> {
    let gap_total = gap_total(items.len(), raw(constraints.main_gap))?;
    budget.charge(items.len())?;
    let hypothetical_outer = sum_item_sizes(items, gap_total, |item| item.hypothetical)?;
    let available = constraints.main_size.map_or(hypothetical_outer, raw);
    let using_grow = hypothetical_outer < available;

    budget.charge(items.len())?;
    for item in items.iter_mut() {
        item.target = raw(item.input.base_main);
        item.frozen = false;
    }

    budget.charge(items.len())?;
    for item in items.iter_mut() {
        let factor_is_zero = if using_grow {
            item.input.grow.millionths() == 0
        } else {
            item.input.shrink.millionths() == 0
        };
        let base = raw(item.input.base_main);
        let inflexible = factor_is_zero
            || (using_grow && base > item.hypothetical)
            || (!using_grow && base < item.hypothetical);
        if inflexible {
            item.target = item.hypothetical;
            item.frozen = true;
        }
    }

    budget.charge(items.len())?;
    let initial_free = remaining_free_space(items, available, gap_total)?;
    let mut iterations = 0_usize;
    let iteration_limit = items
        .len()
        .checked_add(1)
        .ok_or(FlexError::ArithmeticOverflow)?;
    loop {
        budget.charge(items.len())?;
        if !items.iter().any(|item| !item.frozen) {
            break;
        }
        iterations = iterations
            .checked_add(1)
            .ok_or(FlexError::ArithmeticOverflow)?;
        if iterations > iteration_limit {
            return Err(FlexError::ArithmeticOverflow);
        }

        budget.charge(items.len())?;
        let factor_sum =
            items
                .iter()
                .filter(|item| !item.frozen)
                .try_fold(0_i128, |sum, item| {
                    checked_add(
                        sum,
                        i128::from(if using_grow {
                            item.input.grow.millionths()
                        } else {
                            item.input.shrink.millionths()
                        }),
                    )
                })?;
        if factor_sum == 0 {
            budget.charge(items.len())?;
            for item in items.iter_mut().filter(|item| !item.frozen) {
                item.target = item.hypothetical;
                item.frozen = true;
            }
            break;
        }

        budget.charge(items.len())?;
        let mut free = remaining_free_space(items, available, gap_total)?;
        if factor_sum < i128::from(FlexFactor::ONE.millionths()) {
            let scaled =
                checked_mul(initial_free, factor_sum)? / i128::from(FlexFactor::ONE.millionths());
            if scaled.abs() < free.abs() {
                free = scaled;
            }
        }

        budget.charge(items.len())?;
        let weight_sum = flex_weight_sum(items, using_grow)?;
        if weight_sum == 0 {
            budget.charge(items.len())?;
            for item in items.iter_mut().filter(|item| !item.frozen) {
                item.target = item.hypothetical;
                item.frozen = true;
            }
            break;
        }

        // Cumulative division assigns integer-app-unit remainder
        // deterministically while preserving the exact distributed total.
        budget.charge(items.len())?;
        let mut cumulative_weight = 0_i128;
        let mut cumulative_share = 0_i128;
        for item in items.iter_mut().filter(|item| !item.frozen) {
            cumulative_weight = checked_add(cumulative_weight, flex_weight(item, using_grow)?)?;
            let next_cumulative_share = checked_mul(free, cumulative_weight)? / weight_sum;
            let share = checked_sub(next_cumulative_share, cumulative_share)?;
            cumulative_share = next_cumulative_share;
            item.target = checked_add(raw(item.input.base_main), share)?;
        }

        budget.charge(items.len())?;
        let mut total_violation = 0_i128;
        let mut violations = Vec::new();
        violations
            .try_reserve_exact(items.len())
            .map_err(|_| FlexError::AllocationFailed {
                resource: "flex violations",
                requested: items.len(),
            })?;
        for item in items.iter_mut() {
            if item.frozen {
                violations.push(0);
                continue;
            }
            let unclamped = item.target;
            item.target = clamp_main(
                unclamped,
                raw(item.input.min_main),
                item.input.max_main.map(raw),
            );
            let violation = checked_sub(item.target, unclamped)?;
            total_violation = checked_add(total_violation, violation)?;
            violations.push(violation);
        }

        budget.charge(items.len())?;
        for (item, violation) in items.iter_mut().zip(violations) {
            if item.frozen {
                continue;
            }
            let freeze = total_violation == 0
                || (total_violation > 0 && violation > 0)
                || (total_violation < 0 && violation < 0);
            if freeze {
                item.frozen = true;
            }
        }
    }

    budget.charge(items.len())?;
    let used = sum_item_sizes(items, gap_total, |item| item.target)?;
    line.main_size = constraints.main_size.map_or(used, raw);
    Ok(())
}

fn resolve_cross_sizes(
    items: &[WorkingItem],
    lines: &mut [FlexLine],
    constraints: FlexConstraints,
    budget: &mut FlexWorkBudget,
) -> Result<(), FlexError> {
    budget.charge(items.len())?;
    for line in lines.iter_mut() {
        line.cross_size = items[line.start..line.end]
            .iter()
            .map(|item| {
                checked_add(
                    clamp_main(
                        raw(item.input.base_cross),
                        raw(item.input.min_cross),
                        item.input.max_cross.map(raw),
                    ),
                    raw(item.input.outer_cross),
                )
            })
            .try_fold(0_i128, |largest, size| Ok(largest.max(size?)))?;
    }

    if let Some(cross_size) = constraints.cross_size {
        let available = raw(cross_size);
        if lines.len() == 1 {
            budget.charge(1)?;
            lines[0].cross_size = available;
        } else {
            budget.charge(lines.len())?;
            let natural = extent_with_gaps(
                lines.iter().map(|line| line.cross_size),
                raw(constraints.cross_gap),
            )?;
            if natural < available {
                let extra = checked_sub(available, natural)?;
                budget.charge(lines.len())?;
                let count =
                    i128::try_from(lines.len()).map_err(|_| FlexError::ArithmeticOverflow)?;
                let mut distributed = 0_i128;
                for (index, line) in lines.iter_mut().enumerate() {
                    let cumulative = checked_mul(
                        extra,
                        i128::try_from(index.checked_add(1).ok_or(FlexError::ArithmeticOverflow)?)
                            .map_err(|_| FlexError::ArithmeticOverflow)?,
                    )? / count;
                    line.cross_size =
                        checked_add(line.cross_size, checked_sub(cumulative, distributed)?)?;
                    distributed = cumulative;
                }
            }
        }
    }

    budget.charge(lines.len())?;
    let mut cross_offset = 0_i128;
    let line_count = lines.len();
    for (index, line) in lines.iter_mut().enumerate() {
        line.cross_offset = cross_offset;
        cross_offset = checked_add(cross_offset, line.cross_size)?;
        if index.checked_add(1).ok_or(FlexError::ArithmeticOverflow)? < line_count {
            cross_offset = checked_add(cross_offset, raw(constraints.cross_gap))?;
        }
    }
    Ok(())
}

fn append_line_placements(
    items: &[WorkingItem],
    line: FlexLine,
    constraints: FlexConstraints,
    placements: &mut Vec<FlexPlacement>,
) -> Result<(), FlexError> {
    let gap_total = gap_total(items.len(), raw(constraints.main_gap))?;
    let used = sum_item_sizes(items, gap_total, |item| item.target)?;
    let free = checked_sub(line.main_size, used)?;
    let (initial, distributed_gap) =
        justify_offsets(constraints.justify_content, free, items.len())?;
    let mut main_offset = initial;
    for (index, item) in items.iter().enumerate() {
        let unclamped_cross = if item.input.align == AlignItems::Stretch && item.input.cross_auto {
            checked_sub(line.cross_size, raw(item.input.outer_cross))?.max(0)
        } else {
            raw(item.input.base_cross)
        };
        let target_cross = clamp_main(
            unclamped_cross,
            raw(item.input.min_cross),
            item.input.max_cross.map(raw),
        );
        let cross_free = checked_sub(
            checked_sub(line.cross_size, target_cross)?,
            raw(item.input.outer_cross),
        )?;
        let cross_packing = match item.input.align {
            AlignItems::Stretch | AlignItems::Start => 0,
            AlignItems::End => cross_free,
            AlignItems::Center => cross_free / 2,
        };
        placements.push(FlexPlacement {
            source_index: item.input.source_index,
            target_main: checked_au(item.target)?,
            target_cross: checked_au(target_cross)?,
            outer_main_offset: checked_signed_au(main_offset)?,
            outer_cross_offset: checked_signed_au(checked_add(line.cross_offset, cross_packing)?)?,
        });
        main_offset = checked_add(
            main_offset,
            checked_add(item.target, raw(item.input.outer_main))?,
        )?;
        if index.checked_add(1).ok_or(FlexError::ArithmeticOverflow)? < items.len() {
            main_offset = checked_add(main_offset, raw(constraints.main_gap))?;
            main_offset = checked_add(main_offset, distributed_gap)?;
        }
    }
    Ok(())
}

fn justify_offsets(
    justify: JustifyContent,
    free: i128,
    item_count: usize,
) -> Result<(i128, i128), FlexError> {
    if item_count == 0 {
        return Ok((0, 0));
    }
    let count = i128::try_from(item_count).map_err(|_| FlexError::ArithmeticOverflow)?;
    Ok(match justify {
        JustifyContent::Start => (0, 0),
        JustifyContent::End => (free, 0),
        JustifyContent::Center => (free / 2, 0),
        JustifyContent::SpaceBetween if item_count > 1 && free > 0 => {
            (0, free / checked_sub(count, 1)?)
        }
        JustifyContent::SpaceAround if free > 0 => (free / count / 2, free / count),
        JustifyContent::SpaceEvenly if free > 0 => {
            let slots = checked_add(count, 1)?;
            (free / slots, free / slots)
        }
        JustifyContent::SpaceBetween => (0, 0),
        JustifyContent::SpaceAround | JustifyContent::SpaceEvenly => (0, 0),
    })
}

fn remaining_free_space(
    items: &[WorkingItem],
    available: i128,
    gap_total: i128,
) -> Result<i128, FlexError> {
    let used = sum_item_sizes(items, gap_total, |item| {
        if item.frozen {
            item.target
        } else {
            raw(item.input.base_main)
        }
    })?;
    checked_sub(available, used)
}

fn sum_item_sizes(
    items: &[WorkingItem],
    gap_total: i128,
    size: impl Fn(&WorkingItem) -> i128,
) -> Result<i128, FlexError> {
    items.iter().try_fold(gap_total, |sum, item| {
        checked_add(checked_add(sum, size(item))?, raw(item.input.outer_main))
    })
}

fn flex_weight_sum(items: &[WorkingItem], using_grow: bool) -> Result<i128, FlexError> {
    items
        .iter()
        .filter(|item| !item.frozen)
        .try_fold(0_i128, |sum, item| {
            checked_add(sum, flex_weight(item, using_grow)?)
        })
}

fn flex_weight(item: &WorkingItem, using_grow: bool) -> Result<i128, FlexError> {
    let factor = i128::from(if using_grow {
        item.input.grow.millionths()
    } else {
        item.input.shrink.millionths()
    });
    if using_grow {
        Ok(factor)
    } else {
        checked_mul(factor, raw(item.input.base_main))
    }
}

fn gap_total(item_count: usize, gap: i128) -> Result<i128, FlexError> {
    let count = item_count.saturating_sub(1);
    checked_mul(
        i128::try_from(count).map_err(|_| FlexError::ArithmeticOverflow)?,
        gap,
    )
}

fn extent_with_gaps(sizes: impl Iterator<Item = i128>, gap: i128) -> Result<i128, FlexError> {
    let mut extent = 0_i128;
    let mut count = 0_usize;
    for size in sizes {
        extent = checked_add(extent, size)?;
        count = count.checked_add(1).ok_or(FlexError::ArithmeticOverflow)?;
    }
    checked_add(extent, gap_total(count, gap)?)
}

fn clamp_main(value: i128, minimum: i128, maximum: Option<i128>) -> i128 {
    let mut used = value.max(0);
    if let Some(maximum) = maximum {
        used = used.min(maximum);
    }
    // CSS sizing gives an explicit minimum precedence when min > max.
    used.max(minimum)
}

fn sort_work(len: usize) -> Result<usize, FlexError> {
    if len < 2 {
        return Ok(len);
    }
    let bits = usize::BITS
        .checked_sub(
            len.checked_sub(1)
                .ok_or(FlexError::ArithmeticOverflow)?
                .leading_zeros(),
        )
        .ok_or(FlexError::ArithmeticOverflow)?;
    len.checked_mul(
        (bits as usize)
            .checked_add(1)
            .ok_or(FlexError::ArithmeticOverflow)?,
    )
    .ok_or(FlexError::ArithmeticOverflow)
}

const fn raw(value: Au) -> i128 {
    value.raw() as i128
}

fn checked_au(value: i128) -> Result<Au, FlexError> {
    if value < 0 {
        return Err(FlexError::ArithmeticOverflow);
    }
    checked_signed_au(value)
}

fn checked_signed_au(value: i128) -> Result<Au, FlexError> {
    if value < i128::from(i32::MIN) || value > i128::from(i32::MAX) {
        return Err(FlexError::ArithmeticOverflow);
    }
    Ok(Au::from_raw(
        i32::try_from(value).map_err(|_| FlexError::ArithmeticOverflow)?,
    ))
}

fn checked_add(left: i128, right: i128) -> Result<i128, FlexError> {
    left.checked_add(right).ok_or(FlexError::ArithmeticOverflow)
}

fn checked_sub(left: i128, right: i128) -> Result<i128, FlexError> {
    left.checked_sub(right).ok_or(FlexError::ArithmeticOverflow)
}

fn checked_mul(left: i128, right: i128) -> Result<i128, FlexError> {
    left.checked_mul(right).ok_or(FlexError::ArithmeticOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(index: usize, base: i32) -> FlexItemInput {
        FlexItemInput {
            source_index: index,
            order: 0,
            base_main: Au::from_px(base),
            min_main: Au::ZERO,
            max_main: None,
            grow: FlexFactor::default(),
            shrink: FlexFactor::ONE,
            outer_main: Au::ZERO,
            base_cross: Au::from_px(10),
            min_cross: Au::ZERO,
            max_cross: None,
            outer_cross: Au::ZERO,
            cross_auto: false,
            align: AlignItems::Start,
        }
    }

    fn constraints(width: i32) -> FlexConstraints {
        FlexConstraints {
            main_size: Some(Au::from_px(width)),
            cross_size: None,
            wrap: FlexWrap::NoWrap,
            main_gap: Au::ZERO,
            cross_gap: Au::ZERO,
            justify_content: JustifyContent::Start,
            max_lines: 16,
        }
    }

    #[test]
    fn grow_and_shrink_distribute_in_exact_app_units() {
        let mut growing = [item(0, 10), item(1, 10)];
        growing[0].grow = FlexFactor::ONE;
        growing[1].grow = FlexFactor::ONE;
        let plan =
            plan_flex_layout(&growing, constraints(30), &mut FlexWorkBudget::new(1_000)).unwrap();
        assert_eq!(plan.placements[0].target_main, Au::from_px(15));
        assert_eq!(plan.placements[1].target_main, Au::from_px(15));

        let shrinking = [item(0, 20), item(1, 10)];
        let plan =
            plan_flex_layout(&shrinking, constraints(15), &mut FlexWorkBudget::new(1_000)).unwrap();
        assert_eq!(plan.placements[0].target_main, Au::from_px(10));
        assert_eq!(plan.placements[1].target_main, Au::from_px(5));
    }

    #[test]
    fn clamp_freezing_redistributes_remaining_space() {
        let mut items = [item(0, 10), item(1, 10)];
        items[0].grow = FlexFactor::ONE;
        items[1].grow = FlexFactor::ONE;
        items[0].max_main = Some(Au::from_px(12));
        let plan =
            plan_flex_layout(&items, constraints(30), &mut FlexWorkBudget::new(1_000)).unwrap();
        assert_eq!(plan.placements[0].target_main, Au::from_px(12));
        assert_eq!(plan.placements[1].target_main, Au::from_px(18));
    }

    #[test]
    fn work_is_rejected_before_the_first_sort_effect() {
        let inputs = [item(0, 10), item(1, 10)];
        assert_eq!(
            plan_flex_layout(&inputs, constraints(20), &mut FlexWorkBudget::new(1)),
            Err(FlexError::WorkLimitExceeded { limit: 1 })
        );
    }

    #[test]
    fn automatic_cross_extent_includes_cross_axis_minimums() {
        let mut input = item(0, 10);
        input.min_cross = Au::from_px(40);
        let plan =
            plan_flex_layout(&[input], constraints(10), &mut FlexWorkBudget::new(1_000)).unwrap();
        assert_eq!(plan.cross_extent, Au::from_px(40));
        assert_eq!(plan.placements[0].target_cross, Au::from_px(40));
    }

    #[test]
    fn plan_extent_overflow_fails_typed() {
        let mut constraints = constraints(1);
        constraints.main_size = None;
        let inputs = [
            item(0, i32::MAX / Au::PER_CSS_PX),
            item(1, i32::MAX / Au::PER_CSS_PX),
        ];
        assert_eq!(
            plan_flex_layout(&inputs, constraints, &mut FlexWorkBudget::new(1_000)),
            Err(FlexError::ArithmeticOverflow)
        );
    }

    #[test]
    fn positional_alignment_retains_signed_overflow_offsets() {
        let mut inputs = [item(0, 60), item(1, 60)];
        for input in &mut inputs {
            input.shrink = FlexFactor::default();
            input.base_cross = Au::from_px(40);
            input.align = AlignItems::Center;
        }
        let mut constraints = constraints(100);
        constraints.cross_size = Some(Au::from_px(20));
        constraints.justify_content = JustifyContent::Center;
        let plan = plan_flex_layout(&inputs, constraints, &mut FlexWorkBudget::new(1_000)).unwrap();
        assert_eq!(plan.placements[0].outer_main_offset, Au::from_px(-10));
        assert_eq!(plan.placements[1].outer_main_offset, Au::from_px(50));
        assert_eq!(plan.placements[0].outer_cross_offset, Au::from_px(-10));
    }

    #[test]
    fn empty_container_reserves_its_single_generated_line() {
        assert_eq!(required_line_capacity(0, 0), 0);
        assert_eq!(required_line_capacity(0, 1), 1);
        assert_eq!(required_line_capacity(0, 8), 1);
        assert_eq!(required_line_capacity(3, 2), 2);

        let plan = plan_flex_layout(&[], constraints(10), &mut FlexWorkBudget::new(100)).unwrap();
        assert!(plan.placements.is_empty());
        assert_eq!(plan.main_extent, Au::from_px(10));
    }
}
