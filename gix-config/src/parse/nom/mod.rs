use std::borrow::Cow;

use bstr::{BStr, ByteSlice};
use winnow::{
    combinator::alt,
    combinator::delimited,
    combinator::fold_repeat,
    combinator::opt,
    combinator::preceded,
    combinator::repeat,
    error::{ErrorKind, InputError as NomError, ParserError as _},
    prelude::*,
    stream::AsChar,
    token::{one_of, take_till0, take_while},
};

use crate::parse::{error::ParseNode, section, Comment, Error, Event};

/// Attempt to zero-copy parse the provided bytes, passing results to `dispatch`.
pub fn from_bytes<'i>(input: &'i [u8], mut dispatch: impl FnMut(Event<'i>)) -> Result<(), Error> {
    let bom = unicode_bom::Bom::from(input);
    let mut newlines = 0;
    let (i, _) = fold_repeat(
        0..,
        alt((
            comment.map(Event::Comment),
            take_spaces1.map(|whitespace| Event::Whitespace(Cow::Borrowed(whitespace))),
            |i| {
                let (i, (newline, counter)) = take_newlines1.parse_next(i)?;
                newlines += counter;
                let o = Event::Newline(Cow::Borrowed(newline));
                Ok((i, o))
            },
        )),
        || (),
        |_acc, event| dispatch(event),
    )
    .parse_next(&input[bom.len()..])
    // I don't think this can panic. many0 errors if the child parser returns
    // a success where the input was not consumed, but alt will only return Ok
    // if one of its children succeed. However, all of it's children are
    // guaranteed to consume something if they succeed, so the Ok(i) == i case
    // can never occur.
    .expect("many0(alt(...)) panicked. Likely a bug in one of the children parsers.");

    if i.is_empty() {
        return Ok(());
    }

    let mut node = ParseNode::SectionHeader;

    let res = fold_repeat(
        1..,
        |i| section(i, &mut node, &mut dispatch),
        || (),
        |_acc, additional_newlines| {
            newlines += additional_newlines;
        },
    )
    .parse_next(i);
    let (i, _) = res.map_err(|_| Error {
        line_number: newlines,
        last_attempted_parser: node,
        parsed_until: i.as_bstr().into(),
    })?;

    // This needs to happen after we collect sections, otherwise the line number
    // will be off.
    if !i.is_empty() {
        return Err(Error {
            line_number: newlines,
            last_attempted_parser: node,
            parsed_until: i.as_bstr().into(),
        });
    }

    Ok(())
}

fn comment(i: &[u8]) -> IResult<&[u8], Comment<'_>> {
    (
        one_of([';', '#']).map(|tag| tag as u8),
        take_till0(|c| c == b'\n').map(|text: &[u8]| Cow::Borrowed(text.as_bstr())),
    )
        .map(|(tag, text)| Comment { tag, text })
        .parse_next(i)
}

#[cfg(test)]
mod tests;

fn section<'i>(i: &'i [u8], node: &mut ParseNode, dispatch: &mut impl FnMut(Event<'i>)) -> IResult<&'i [u8], usize> {
    let (mut i, header) = section_header(i)?;
    dispatch(Event::SectionHeader(header));

    let mut newlines = 0;

    // This would usually be a many0(alt(...)), the manual loop allows us to
    // optimize vec insertions
    loop {
        let old_i = i;

        if let (new_i, Some(v)) = opt(take_spaces1).parse_next(i)? {
            i = new_i;
            dispatch(Event::Whitespace(Cow::Borrowed(v.as_bstr())));
        }

        if let (new_i, Some((v, new_newlines))) = opt(take_newlines1).parse_next(i)? {
            i = new_i;
            newlines += new_newlines;
            dispatch(Event::Newline(Cow::Borrowed(v.as_bstr())));
        }

        if let Ok((new_i, new_newlines)) = key_value_pair(i, node, dispatch) {
            i = new_i;
            newlines += new_newlines;
        }

        if let (new_i, Some(comment)) = opt(comment).parse_next(i)? {
            i = new_i;
            dispatch(Event::Comment(comment));
        }

        if old_i == i {
            break;
        }
    }

    Ok((i, newlines))
}

fn section_header(i: &[u8]) -> IResult<&[u8], section::Header<'_>> {
    // No spaces must be between section name and section start
    let (i, name) = preceded('[', take_while(1.., is_section_char).map(|name: &[u8]| name.as_bstr())).parse_next(i)?;

    if let (i, Some(_)) = opt(one_of::<_, _, NomError<&[u8]>>(']')).parse_next(i)? {
        // Either section does not have a subsection or using deprecated
        // subsection syntax at this point.
        let header = match memchr::memrchr(b'.', name.as_bytes()) {
            Some(index) => section::Header {
                name: section::Name(Cow::Borrowed(name[..index].as_bstr())),
                separator: name.get(index..=index).map(|s| Cow::Borrowed(s.as_bstr())),
                subsection_name: name.get(index + 1..).map(|s| Cow::Borrowed(s.as_bstr())),
            },
            None => section::Header {
                name: section::Name(Cow::Borrowed(name.as_bstr())),
                separator: None,
                subsection_name: None,
            },
        };

        if header.name.is_empty() {
            return Err(winnow::error::ErrMode::from_error_kind(i, ErrorKind::Fail));
        }
        return Ok((i, header));
    }

    // Section header must be using modern subsection syntax at this point.
    (take_spaces1, delimited('"', opt(sub_section), "\"]"))
        .map(|(whitespace, subsection_name)| section::Header {
            name: section::Name(Cow::Borrowed(name)),
            separator: Some(Cow::Borrowed(whitespace)),
            subsection_name,
        })
        .parse_next(i)
}

fn is_section_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'.'
}

fn sub_section(mut i: &[u8]) -> IResult<&[u8], Cow<'_, BStr>> {
    let mut output = Cow::Borrowed(Default::default());
    if let (next_i, Some(sub)) = opt(subsection_subset).parse_next(i)? {
        i = next_i;
        output = Cow::Borrowed(sub.as_bstr());
    }
    while let (next_i, Some(sub)) = opt(subsection_subset).parse_next(i)? {
        i = next_i;
        output.to_mut().extend(sub);
    }

    Ok((i, output))
}

fn subsection_subset(i: &[u8]) -> IResult<&[u8], &[u8]> {
    alt((subsection_unescaped, subsection_escaped_char)).parse_next(i)
}

fn subsection_unescaped(i: &[u8]) -> IResult<&[u8], &[u8]> {
    take_while(1.., is_subsection_unescaped_char).parse_next(i)
}

fn subsection_escaped_char(i: &[u8]) -> IResult<&[u8], &[u8]> {
    preceded('\\', one_of(is_subsection_escapeable_char).recognize()).parse_next(i)
}

fn is_subsection_escapeable_char(c: u8) -> bool {
    c != b'\n'
}

fn is_subsection_unescaped_char(c: u8) -> bool {
    c != b'"' && c != b'\\' && c != b'\n' && c != 0
}

fn key_value_pair<'i>(
    i: &'i [u8],
    node: &mut ParseNode,
    dispatch: &mut impl FnMut(Event<'i>),
) -> IResult<&'i [u8], usize> {
    *node = ParseNode::Name;
    let (i, name) = config_name(i)?;

    dispatch(Event::SectionKey(section::Key(Cow::Borrowed(name))));

    let (i, whitespace) = opt(take_spaces1).parse_next(i)?;
    if let Some(whitespace) = whitespace {
        dispatch(Event::Whitespace(Cow::Borrowed(whitespace)));
    }

    *node = ParseNode::Value;
    let (i, newlines) = config_value(i, dispatch)?;
    Ok((i, newlines))
}

/// Parses the config name of a config pair. Assumes the input has already been
/// trimmed of any leading whitespace.
fn config_name(i: &[u8]) -> IResult<&[u8], &BStr> {
    (
        one_of(|c: u8| c.is_ascii_alphabetic()),
        take_while(0.., |c: u8| c.is_ascii_alphanumeric() || c == b'-'),
    )
        .recognize()
        .map(|s: &[u8]| s.as_bstr())
        .parse_next(i)
}

fn config_value<'i>(i: &'i [u8], dispatch: &mut impl FnMut(Event<'i>)) -> IResult<&'i [u8], usize> {
    if let (i, Some(_)) = opt('=').parse_next(i)? {
        dispatch(Event::KeyValueSeparator);
        let (i, whitespace) = opt(take_spaces1).parse_next(i)?;
        if let Some(whitespace) = whitespace {
            dispatch(Event::Whitespace(Cow::Borrowed(whitespace)));
        }
        let (i, newlines) = value_impl(i, dispatch)?;
        Ok((i, newlines))
    } else {
        // This is a special way of denoting 'empty' values which a lot of code depends on.
        // Hence, rather to fix this everywhere else, leave it here and fix it where it matters, namely
        // when it's about differentiating between a missing key-value separator, and one followed by emptiness.
        dispatch(Event::Value(Cow::Borrowed("".into())));
        Ok((i, 0))
    }
}

/// Handles parsing of known-to-be values. This function handles both single
/// line values as well as values that are continuations.
fn value_impl<'i>(i: &'i [u8], dispatch: &mut impl FnMut(Event<'i>)) -> IResult<&'i [u8], usize> {
    let start_i = i;
    let mut value_end = None;
    let mut value_start: usize = 0;
    let mut newlines = 0;

    // This is required to ignore comment markers if they're in a quote.
    let mut is_in_quotes = false;
    // Used to determine if we return a Value or Value{Not,}Done
    let mut partial_value_found = false;
    let mut current_index: usize = 0;

    let mut bytes = i.iter();
    while let Some(c) = bytes.next() {
        match c {
            b'\n' => {
                value_end = Some(current_index);
                break;
            }
            b';' | b'#' if !is_in_quotes => {
                value_end = Some(current_index);
                break;
            }
            b'\\' => {
                current_index += 1;
                let Some(mut c) = bytes.next() else {
                    return Err(winnow::error::ErrMode::from_error_kind(start_i, ErrorKind::Token));
                };
                let escape_index = current_index - 1;
                let escaped_index = current_index;
                let mut consumed = 1;
                if *c == b'\r' {
                    current_index += 1;
                    c = bytes
                        .next()
                        .ok_or_else(|| winnow::error::ErrMode::from_error_kind(start_i, ErrorKind::Token))?;
                    if *c != b'\n' {
                        return Err(winnow::error::ErrMode::from_error_kind(start_i, ErrorKind::Slice));
                    }
                    consumed += 1;
                }

                match c {
                    b'\n' => {
                        partial_value_found = true;
                        dispatch(Event::ValueNotDone(Cow::Borrowed(
                            i[value_start..escape_index].as_bstr(),
                        )));
                        let nl_end = escaped_index + consumed;
                        dispatch(Event::Newline(Cow::Borrowed(i[escaped_index..nl_end].as_bstr())));
                        value_start = nl_end;
                        value_end = None;
                        newlines += 1;
                    }
                    b'n' | b't' | b'\\' | b'b' | b'"' => {}
                    _ => {
                        return Err(winnow::error::ErrMode::from_error_kind(start_i, ErrorKind::Token));
                    }
                }
            }
            b'"' => is_in_quotes = !is_in_quotes,
            _ => {}
        }
        current_index += 1;
    }
    if is_in_quotes {
        return Err(winnow::error::ErrMode::from_error_kind(start_i, ErrorKind::Slice));
    }
    let last_value_index = current_index;

    let value_end = match value_end {
        None => {
            if last_value_index == 0 {
                dispatch(Event::Value(Cow::Borrowed("".into())));
                return Ok((&i[0..], newlines));
            } else {
                i.len()
            }
        }
        Some(idx) => idx,
    };

    let i = &i[value_start..];
    let value_end = value_end - value_start;
    let value_end_no_trailing_whitespace = i[..value_end]
        .iter()
        .enumerate()
        .rev()
        .find_map(|(idx, b)| (!b.is_ascii_whitespace()).then_some(idx + 1))
        .unwrap_or(0);
    let remainder_value = &i[..value_end_no_trailing_whitespace];
    let i = &i[value_end_no_trailing_whitespace..];

    if partial_value_found {
        dispatch(Event::ValueDone(Cow::Borrowed(remainder_value.as_bstr())));
    } else {
        dispatch(Event::Value(Cow::Borrowed(remainder_value.as_bstr())));
    }

    Ok((i, newlines))
}

fn take_spaces1(i: &[u8]) -> IResult<&[u8], &BStr> {
    take_while(1.., |c: u8| c.is_space())
        .map(|spaces: &[u8]| spaces.as_bstr())
        .parse_next(i)
}

fn take_newlines1(i: &[u8]) -> IResult<&[u8], (&BStr, usize)> {
    repeat(1.., alt(("\r\n", "\n")))
        .with_recognized()
        .map(|(count, newlines): (usize, &[u8])| (newlines.as_bstr(), count))
        .parse_next(i)
}
