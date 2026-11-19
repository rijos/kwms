//! OWS 1.1 common capabilities blocks, shared by the OGC protocols:
//! `ows:ServiceIdentification`, `ows:ServiceProvider` and
//! `ows:OperationsMetadata`, filled from the `service:` config section.
//!
//! Element order follows the OWS 1.1 schema (owsServiceIdentification.xsd,
//! owsServiceProvider.xsd, owsOperationsMetadata.xsd); the shape matches
//! PDOK's WMTS documents.

use anyhow::{Context, Result};
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use crate::config::{ServiceContact, ServiceMetadata};

/// `<tag>text</tag>`.
pub(crate) fn write_text<W: std::io::Write>(writer: &mut Writer<W>, tag: &str, text: &str) -> Result<()> {
    writer
        .write_event(Event::Start(BytesStart::new(tag)))
        .with_context(|| format!("start <{tag}>"))?;
    writer
        .write_event(Event::Text(BytesText::new(text)))
        .with_context(|| format!("text for <{tag}>"))?;
    writer
        .write_event(Event::End(BytesEnd::new(tag)))
        .with_context(|| format!("end <{tag}>"))?;
    Ok(())
}

pub(crate) fn start<W: std::io::Write>(writer: &mut Writer<W>, tag: &str) -> Result<()> {
    writer
        .write_event(Event::Start(BytesStart::new(tag)))
        .with_context(|| format!("start <{tag}>"))?;
    Ok(())
}

pub(crate) fn end<W: std::io::Write>(writer: &mut Writer<W>, tag: &str) -> Result<()> {
    writer
        .write_event(Event::End(BytesEnd::new(tag)))
        .with_context(|| format!("end <{tag}>"))?;
    Ok(())
}

/// `<tag xlink:href="href"/>`.
pub(crate) fn write_href<W: std::io::Write>(writer: &mut Writer<W>, tag: &str, href: &str) -> Result<()> {
    let mut el = BytesStart::new(tag);
    el.push_attribute(("xlink:href", href));
    writer
        .write_event(Event::Empty(el))
        .with_context(|| format!("<{tag}>"))?;
    Ok(())
}

/// `ows:ServiceIdentification`. `default_title` is used when the config
/// sets none.
pub fn write_service_identification<W: std::io::Write>(
    writer: &mut Writer<W>,
    service: &ServiceMetadata,
    default_title: &str,
    service_type: &str,
    version: &str,
) -> Result<()> {
    start(writer, "ows:ServiceIdentification")?;
    write_text(writer, "ows:Title", service.title.as_deref().unwrap_or(default_title))?;
    if let Some(a) = &service.abstract_ {
        write_text(writer, "ows:Abstract", a)?;
    }
    if !service.keywords.is_empty() {
        start(writer, "ows:Keywords")?;
        for k in &service.keywords {
            write_text(writer, "ows:Keyword", k)?;
        }
        end(writer, "ows:Keywords")?;
    }
    write_text(writer, "ows:ServiceType", service_type)?;
    write_text(writer, "ows:ServiceTypeVersion", version)?;
    if let Some(f) = &service.fees {
        write_text(writer, "ows:Fees", f)?;
    }
    if let Some(c) = &service.access_constraints {
        write_text(writer, "ows:AccessConstraints", c)?;
    }
    end(writer, "ows:ServiceIdentification")
}

/// `ows:ServiceProvider` (name defaults to "kwms").
pub fn write_service_provider<W: std::io::Write>(writer: &mut Writer<W>, service: &ServiceMetadata) -> Result<()> {
    let p = &service.provider;
    start(writer, "ows:ServiceProvider")?;
    write_text(writer, "ows:ProviderName", p.name.as_deref().unwrap_or("kwms"))?;
    if let Some(site) = &p.site {
        write_href(writer, "ows:ProviderSite", site)?;
    }
    if let Some(c) = &p.contact {
        write_contact(writer, c)?;
    }
    end(writer, "ows:ServiceProvider")
}

fn write_contact<W: std::io::Write>(writer: &mut Writer<W>, c: &ServiceContact) -> Result<()> {
    let text = |v: &Option<String>| v.clone().unwrap_or_default();
    start(writer, "ows:ServiceContact")?;
    write_text(writer, "ows:IndividualName", &text(&c.individual_name))?;
    write_text(writer, "ows:PositionName", &text(&c.position_name))?;
    start(writer, "ows:ContactInfo")?;
    start(writer, "ows:Phone")?;
    write_text(writer, "ows:Voice", &text(&c.voice))?;
    write_text(writer, "ows:Facsimile", &text(&c.facsimile))?;
    end(writer, "ows:Phone")?;
    start(writer, "ows:Address")?;
    write_text(writer, "ows:DeliveryPoint", &text(&c.delivery_point))?;
    write_text(writer, "ows:City", &text(&c.city))?;
    write_text(writer, "ows:AdministrativeArea", &text(&c.administrative_area))?;
    write_text(writer, "ows:PostalCode", &text(&c.postal_code))?;
    write_text(writer, "ows:Country", &text(&c.country))?;
    write_text(writer, "ows:ElectronicMailAddress", &text(&c.email))?;
    end(writer, "ows:Address")?;
    if let Some(h) = &c.hours_of_service {
        write_text(writer, "ows:HoursOfService", h)?;
    }
    if let Some(i) = &c.contact_instructions {
        write_text(writer, "ows:ContactInstructions", i)?;
    }
    end(writer, "ows:ContactInfo")?;
    end(writer, "ows:ServiceContact")
}

/// One HTTP GET binding of an operation.
pub struct GetBinding<'a> {
    pub href: &'a str,
    /// `GetEncoding` constraint value: `KVP` or `RESTful`.
    pub encoding: &'a str,
}

/// `ows:OperationsMetadata`: each operation with its GET bindings.
pub fn write_operations_metadata<W: std::io::Write>(
    writer: &mut Writer<W>,
    operations: &[&str],
    bindings: &[GetBinding<'_>],
) -> Result<()> {
    start(writer, "ows:OperationsMetadata")?;
    for op in operations {
        let mut el = BytesStart::new("ows:Operation");
        el.push_attribute(("name", *op));
        writer.write_event(Event::Start(el)).context("start Operation")?;
        start(writer, "ows:DCP")?;
        start(writer, "ows:HTTP")?;
        for b in bindings {
            let mut get = BytesStart::new("ows:Get");
            get.push_attribute(("xlink:href", b.href));
            writer.write_event(Event::Start(get)).context("start Get")?;
            let mut constraint = BytesStart::new("ows:Constraint");
            constraint.push_attribute(("name", "GetEncoding"));
            writer
                .write_event(Event::Start(constraint))
                .context("start Constraint")?;
            start(writer, "ows:AllowedValues")?;
            write_text(writer, "ows:Value", b.encoding)?;
            end(writer, "ows:AllowedValues")?;
            end(writer, "ows:Constraint")?;
            end(writer, "ows:Get")?;
        }
        end(writer, "ows:HTTP")?;
        end(writer, "ows:DCP")?;
        end(writer, "ows:Operation")?;
    }
    end(writer, "ows:OperationsMetadata")
}
