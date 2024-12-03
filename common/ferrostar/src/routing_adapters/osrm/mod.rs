//! Response parsing for OSRM-compatible JSON (including Stadia Maps, Valhalla, Mapbox, etc.).

pub(crate) mod models;
pub mod utilities;

use super::RouteResponseParser;
use crate::models::{
    AnyAnnotationValue, GeographicCoordinate, Incident, LaneInfo, RouteStep, SpokenInstruction,
    VisualInstruction, VisualInstructionContent, Waypoint, WaypointKind,
};
use crate::routing_adapters::utilities::get_coordinates_from_geometry;
use crate::routing_adapters::{
    osrm::models::{
        Route as OsrmRoute, RouteResponse, RouteStep as OsrmRouteStep, Waypoint as OsrmWaypoint,
    },
    ParsingError, Route,
};
#[cfg(all(not(feature = "std"), feature = "alloc"))]
use alloc::{string::ToString, vec, vec::Vec};
use geo::BoundingRect;
use polyline::decode_polyline;
use utilities::get_annotation_slice;
use uuid::Uuid;

/// A response parser for OSRM-compatible routing backends.
///
/// The parser is NOT limited to only the standard OSRM format; many Valhalla/Mapbox tags are also
/// parsed and are included in the final route.
#[derive(Debug)]
pub struct OsrmResponseParser {
    polyline_precision: u32,
}

impl OsrmResponseParser {
    pub fn new(polyline_precision: u32) -> Self {
        Self { polyline_precision }
    }
}

impl RouteResponseParser for OsrmResponseParser {
    fn parse_response(&self, response: Vec<u8>) -> Result<Vec<Route>, ParsingError> {
        let res: RouteResponse = serde_json::from_slice(&response)?;

        if res.code == "Ok" {
            res.routes
                .iter()
                .map(|route| Route::from_osrm(route, &res.waypoints, self.polyline_precision))
                .collect::<Result<Vec<_>, _>>()
        } else {
            Err(ParsingError::InvalidStatusCode { code: res.code })
        }
    }
}

impl Route {
    pub fn from_osrm(
        route: &OsrmRoute,
        waypoints: &[OsrmWaypoint],
        polyline_precision: u32,
    ) -> Result<Self, ParsingError> {
        let via_waypoint_indices: Vec<_> = route
            .legs
            .iter()
            .flat_map(|leg| leg.via_waypoints.iter().map(|via| via.waypoint_index))
            .collect();

        let waypoints: Vec<_> = waypoints
            .iter()
            .enumerate()
            .map(|(idx, waypoint)| Waypoint {
                coordinate: GeographicCoordinate {
                    lat: waypoint.location.latitude(),
                    lng: waypoint.location.longitude(),
                },
                kind: if via_waypoint_indices.contains(&idx) {
                    WaypointKind::Via
                } else {
                    WaypointKind::Break
                },
            })
            .collect();

        let linestring = decode_polyline(&route.geometry, polyline_precision).map_err(|error| {
            ParsingError::InvalidGeometry {
                error: error.to_string(),
            }
        })?;
        if let Some(bbox) = linestring.bounding_rect() {
            let geometry: Vec<GeographicCoordinate> = linestring
                .coords()
                .map(|coord| GeographicCoordinate::from(*coord))
                .collect();

            let steps = route
                .legs
                .iter()
                .flat_map(|leg| {
                    // Converts all single value annotation vectors into a single vector witih a value object.
                    let annotations = leg
                        .annotation
                        .as_ref()
                        .map(|leg_annotation| utilities::zip_annotations(leg_annotation.clone()));

                    // Convert all incidents into a vector of Incident objects.
                    let incident_items = leg
                        .incidents
                        .iter()
                        .map(Incident::from)
                        .collect::<Vec<Incident>>();

                    // Index for the annotations slice
                    let mut start_index: usize = 0;

                    return leg.steps.iter().map(move |step| {
                        let step_geometry =
                            get_coordinates_from_geometry(&step.geometry, polyline_precision)?;

                        // Slice the annotations for the current step.
                        // The annotations array represents segments between coordinates.
                        //
                        // 1. Annotations should never repeat.
                        // 2. Each step has one less annotation than coordinate.
                        // 3. The last step never has annotations as it's two of the route's last coordinate (duplicate).
                        let step_index_len = step_geometry.len() - 1_usize;
                        let end_index = start_index + step_index_len;

                        let annotation_slice =
                            get_annotation_slice(annotations.clone(), start_index, end_index).ok();

                        let relevant_incidents_slice = incident_items
                            .iter()
                            .filter(|incident| {
                                let incident_start = incident.geometry_index_start as usize;

                                match incident.geometry_index_end {
                                    Some(end) => {
                                        let incident_end = end as usize;
                                        incident_start >= start_index && incident_end <= end_index
                                    }
                                    None => {
                                        incident_start >= start_index && incident_start <= end_index
                                    }
                                }
                            })
                            .map(|incident| {
                                let mut adjusted_incident = incident.clone();
                                if adjusted_incident.geometry_index_start - start_index as u64 > 0 {
                                    adjusted_incident.geometry_index_start -= start_index as u64;
                                } else {
                                    adjusted_incident.geometry_index_start = 0;
                                }

                                if let Some(end) = adjusted_incident.geometry_index_end {
                                    let adjusted_end = end - start_index as u64;
                                    adjusted_incident.geometry_index_end =
                                        Some(if adjusted_end > end_index as u64 {
                                            end_index as u64
                                        } else {
                                            adjusted_end
                                        })
                                }
                                adjusted_incident
                            })
                            .collect::<Vec<Incident>>();

                        start_index = end_index;

                        return RouteStep::from_osrm_and_geom(
                            step,
                            step_geometry,
                            annotation_slice,
                            relevant_incidents_slice,
                        );
                    });
                })
                .collect::<Result<Vec<_>, _>>()?;

            Ok(Route {
                geometry,
                bbox: bbox.into(),
                distance: route.distance,
                waypoints: waypoints.clone(),
                steps,
            })
        } else {
            Err(ParsingError::InvalidGeometry {
                error: "Bounding box could not be calculated".to_string(),
            })
        }
    }
}

impl RouteStep {
    fn from_osrm_and_geom(
        value: &OsrmRouteStep,
        geometry: Vec<GeographicCoordinate>,
        annotations: Option<Vec<AnyAnnotationValue>>,
        incidents: Vec<Incident>,
    ) -> Result<Self, ParsingError> {
        let visual_instructions = value
            .banner_instructions
            .iter()
            .map(|banner| VisualInstruction {
                primary_content: VisualInstructionContent {
                    text: banner.primary.text.clone(),
                    maneuver_type: banner.primary.maneuver_type,
                    maneuver_modifier: banner.primary.maneuver_modifier,
                    roundabout_exit_degrees: banner.primary.roundabout_exit_degrees,
                    lane_info: None,
                },
                secondary_content: banner.secondary.as_ref().map(|secondary| {
                    VisualInstructionContent {
                        text: secondary.text.clone(),
                        maneuver_type: secondary.maneuver_type,
                        maneuver_modifier: secondary.maneuver_modifier,
                        roundabout_exit_degrees: banner.primary.roundabout_exit_degrees,
                        lane_info: None,
                    }
                }),
                sub_content: banner.sub.as_ref().map(|sub| VisualInstructionContent {
                    text: sub.text.clone(),
                    maneuver_type: sub.maneuver_type,
                    maneuver_modifier: sub.maneuver_modifier,
                    roundabout_exit_degrees: sub.roundabout_exit_degrees,
                    lane_info: {
                        let lane_infos: Vec<LaneInfo> = sub
                            .components
                            .iter()
                            .filter(|component| component.component_type.as_deref() == Some("lane"))
                            .map(|component| LaneInfo {
                                active: component.active.unwrap_or(false),
                                directions: component.directions.clone().unwrap_or_default(),
                                active_direction: component.active_direction.clone(),
                            })
                            .collect();

                        if lane_infos.is_empty() {
                            None
                        } else {
                            Some(lane_infos)
                        }
                    },
                }),
                trigger_distance_before_maneuver: banner.distance_along_geometry,
            })
            .collect();

        let spoken_instructions = value
            .voice_instructions
            .iter()
            .map(|instruction| SpokenInstruction {
                text: instruction.announcement.clone(),
                ssml: instruction.ssml_announcement.clone(),
                trigger_distance_before_maneuver: instruction.distance_along_geometry,
                utterance_id: Uuid::new_v4(),
            })
            .collect();

        // Convert the annotations to a vector of json strings.
        // This allows us to safely pass the RouteStep through the FFI boundary.
        // The host platform can then parse an arbitrary annotation object.
        let annotations_as_strings: Option<Vec<String>> = annotations.map(|annotations_vec| {
            annotations_vec
                .iter()
                .map(|annotation| serde_json::to_string(annotation).unwrap())
                .collect()
        });

        Ok(RouteStep {
            geometry,
            // TODO: Investigate using the haversine distance or geodesics to normalize.
            // Valhalla in particular is a bit nonstandard. See https://github.com/valhalla/valhalla/issues/1717
            distance: value.distance,
            duration: value.duration,
            road_name: value.name.clone(),
            instruction: value.maneuver.get_instruction(),
            visual_instructions,
            spoken_instructions,
            annotations: annotations_as_strings,
            incidents,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STANDARD_OSRM_POLYLINE6_RESPONSE: &str = r#"{"code":"Ok","routes":[{"geometry":"qikdcB{~dpXmxRbaBuqAoqKyy@svFwNcfKzsAysMdr@evD`m@qrAohBi}A{OkdGjg@ajDZww@lJ}Jrs@}`CvzBq`E`PiB`~A|l@z@feA","legs":[{"steps":[],"summary":"","weight":263.1,"duration":260.2,"distance":1886.3},{"steps":[],"summary":"","weight":370.5,"duration":370.5,"distance":2845.5}],"weight_name":"routability","weight":633.6,"duration":630.7,"distance":4731.8}],"waypoints":[{"hint":"Dv8JgCp3moUXAAAABQAAAAAAAAAgAAAAIXRPQYXNK0AAAAAAcPePQQsAAAADAAAAAAAAABAAAAA6-wAA_kvMAKlYIQM8TMwArVghAwAA7wrXLH_K","distance":4.231521214,"name":"Friedrichstraße","location":[13.388798,52.517033]},{"hint":"JEvdgVmFiocGAAAACgAAAAAAAAB3AAAAppONQOodwkAAAAAA8TeEQgYAAAAKAAAAAAAAAHcAAAA6-wAAfm7MABiJIQOCbswA_4ghAwAAXwXXLH_K","distance":2.795148358,"name":"Torstraße","location":[13.39763,52.529432]},{"hint":"oSkYgP___38fAAAAUQAAACYAAAAeAAAAeosKQlNOX0IQ7CZCjsMGQh8AAABRAAAAJgAAAB4AAAA6-wAASufMAOdwIQNL58wA03AhAwQAvxDXLH_K","distance":2.226580806,"name":"Platz der Vereinten Nationen","location":[13.428554,52.523239]}]}"#;
    const VALHALLA_OSRM_RESPONSE: &str = r#"{"code":"Ok","routes":[{"distance":2604.35,"duration":2007.289,"geometry":"e|akpBozpfn@AG~ApSAzFg@pKsFvfA]lFdDr@kAvOoAvDkC|B]~DMzAyCj^c@lFi@d@wIbHu@f@cV|PkA~@_TxQxX|eC{Az@qDrBw@b@{BnATbCNjBd@rHyAj@g@JiDrAcJxDcBjBcA^sDvAsIjDmCnD}@R`@bHgHnBsRvGkDhCsDpTpF~dEPfMfAft@H~FNrEdAt}@f@pY@rA?`@@rBJhRCdAIbD]nFa@bDaAbIiAdImB~MKt@wGrd@qBnOoDbUwAxJVfH\\jMHpEGzAiAjDqMbf@gBnFkC~HeDbKs@vBkCtF}CpGuIzNU`@oGzH{FhGqi@hc@ud@t_@wIpI{JfNqLfTwJjVgDdJ_HvYaEpUgHxa@aFhd@mErt@q@~FmFrd@oJdw@kFmDsCyAyArJgAdAJhDm@`G_@fCMrAmAfFiKf|@{Fxh@oCdSi@dGaBrQcBbNwCd\\kGlh@uA~PuEzr@_@bHa@dC}@\\KbEOvCk@FoQbw@uNno@Gv@SxCo@hEiA`@i@nBf@pCQtDk@xC{B|KgTraAuA\\i@o@mFzY}GiGqBoC","legs":[{"admins":[{"iso_3166_1":"EE","iso_3166_1_alpha3":"EST"}],"annotation":{"distance":[0.2,19.4,7.1,11.6,66.4,6.9,9.4,15.7,6.9,8.6,5.7,2.7,29.7,7.0,2.6,20.9,3.2,44.3,4.6,41.1,130.5,5.4,10.4,3.3,7.3,3.9,3.2,9.0,5.2,2.3,9.8,20.5,6.4,3.9,10.3,19.5,9.3,3.5,8.5,16.8,35.8,10.3,21.9,179.8,12.9,48.4,7.3,6.1,56.9,24.2,2.4,1.0,3.3,17.5,2.0,4.7,7.0,5.0,9.9,10.1,14.9,1.7,37.5,16.2,22.3,11.8,8.5,13.1,6.0,2.6,6.4,43.9,8.9,11.9,14.3,4.5,10.4,11.7,23.9,1.6,17.6,15.9,82.6,73.4,21.4,25.3,30.9,29.8,13.8,29.0,23.1,35.6,36.0,49.9,7.8,36.5,54.8,14.0,8.6,11.7,4.5,4.9,7.7,4.2,2.5,7.9,59.6,40.4,20.0,7.8,17.7,14.8,27.7,40.4,17.0,48.4,8.5,4.2,3.6,5.6,4.4,2.5,60.6,52.0,1.6,4.5,6.3,4.2,3.9,4.7,5.2,5.0,13.6,71.2,4.9,2.7,27.7,17.6,7.5],"duration":[0.184,15.315,5.639,9.818,51.539,4.898,6.604,12.227,5.127,6.412,4.012,1.919,20.947,4.96,1.815,14.72,2.267,33.128,3.248,29.012,101.367,3.808,8.841,2.592,5.742,2.774,2.247,6.331,3.643,1.589,6.887,14.472,4.482,2.747,7.288,13.793,6.594,2.469,5.983,13.027,27.83,8.028,15.49,126.908,12.15,34.152,5.129,4.281,42.571,18.073,1.781,0.681,2.318,17.664,2.012,4.717,7.059,5.058,7.669,7.844,10.516,1.177,26.445,11.458,15.741,8.304,5.987,13.246,4.213,1.864,4.77,32.852,6.677,8.938,10.737,3.339,7.818,11.006,22.394,1.32,14.893,13.483,69.994,51.784,15.108,19.969,24.415,23.531,10.899,32.187,16.31,25.104,25.445,35.213,5.477,25.799,38.709,9.902,6.086,8.228,3.156,3.427,5.46,2.993,1.871,5.888,44.617,30.206,15.496,5.487,12.51,10.434,19.585,31.347,13.188,37.62,6.681,3.349,2.809,3.942,3.1,1.737,45.312,40.41,1.278,3.174,4.898,3.284,3.057,3.644,4.072,3.527,13.723,50.263,3.432,1.908,20.727,17.401,7.403],"maxspeed":[{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"speed":30,"unit":"km/h"},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true}],"speed":[1.3,1.3,1.3,1.2,1.3,1.4,1.4,1.3,1.3,1.3,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.3,1.4,1.4,1.3,1.4,1.2,1.3,1.3,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.3,1.3,1.3,1.4,1.4,1.1,1.4,1.4,1.4,1.3,1.3,1.3,1.4,1.4,1.0,1.0,1.0,1.0,1.0,1.3,1.3,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.0,1.4,1.4,1.3,1.3,1.3,1.3,1.3,1.3,1.3,1.1,1.1,1.2,1.2,1.2,1.2,1.4,1.4,1.3,1.3,1.3,1.3,0.9,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.4,1.3,1.3,1.3,1.3,1.3,1.4,1.4,1.4,1.4,1.3,1.3,1.3,1.3,1.3,1.3,1.4,1.4,1.4,1.3,1.3,1.3,1.4,1.3,1.3,1.3,1.3,1.3,1.4,1.0,1.4,1.4,1.4,1.3,1.0,1.0]},"distance":2604.35,"duration":2007.289,"steps":[{"bannerInstructions":[{"distanceAlongGeometry":111.251,"primary":{"components":[{"text":"Turn left onto the walkway.","type":"text"}],"modifier":"left","text":"Turn left onto the walkway.","type":"turn"}}],"distance":111.251,"driving_side":"right","duration":90.107,"geometry":"e|akpBozpfn@AG~ApSAzFg@pKsFvfA]lF","intersections":[{"admin_index":0,"bearings":[254],"duration":20.754,"entry":[true],"geometry_index":0,"location":[24.765368,59.442643],"out":0,"weight":21.791},{"admin_index":0,"bearings":[7,82,189,281],"duration":11.165,"entry":[true,false,true,true],"geometry_index":3,"in":1,"location":[24.764917,59.442597],"out":3,"turn_duration":1.0,"turn_weight":1.0,"weight":12.181},{"admin_index":0,"bearings":[13,101,191,282],"duration":52.247,"entry":[true,false,true,true],"geometry_index":4,"in":1,"location":[24.764716,59.442617],"out":3,"turn_duration":1.0,"turn_weight":1.0,"weight":52.247},{"admin_index":0,"bearings":[49,102,191,284],"entry":[true,false,true,true],"geometry_index":5,"in":1,"location":[24.763568,59.442739],"out":3,"turn_duration":1.0,"turn_weight":1.0}],"maneuver":{"bearing_after":254,"bearing_before":0,"instruction":"Walk west on the walkway.","location":[24.765368,59.442643],"type":"depart"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"Walk west on the walkway.","distanceAlongGeometry":111.251,"ssmlAnnouncement":"<speak>Walk west on the walkway.</speak>"},{"announcement":"In 200 feet, Turn left onto the walkway.","distanceAlongGeometry":60.0,"ssmlAnnouncement":"<speak>In 200 feet, Turn left onto the walkway.</speak>"}],"weight":92.161},{"bannerInstructions":[{"distanceAlongGeometry":9.0,"primary":{"components":[{"text":"Laeva","type":"text"}],"modifier":"right","text":"Laeva","type":"turn"}}],"distance":9.0,"driving_side":"right","duration":6.353,"geometry":"ccbkpBqbmfn@dDr@","intersections":[{"admin_index":0,"bearings":[14,104,189],"entry":[true,false,true],"geometry_index":6,"in":1,"location":[24.763449,59.442754],"out":2}],"maneuver":{"bearing_after":189,"bearing_before":284,"instruction":"Turn left onto the walkway.","location":[24.763449,59.442754],"modifier":"left","type":"turn"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 14 feet, Turn right onto Laeva.","distanceAlongGeometry":4.5,"ssmlAnnouncement":"<speak>In 14 feet, Turn right onto Laeva.</speak>"}],"weight":6.353},{"bannerInstructions":[{"distanceAlongGeometry":16.0,"primary":{"components":[{"text":"Bear right.","type":"text"}],"modifier":"slight right","text":"Bear right.","type":"turn"}}],"distance":16.0,"driving_side":"right","duration":12.424,"geometry":"}}akpB}`mfn@kAvO","intersections":[{"admin_index":0,"bearings":[9,101,200,286],"entry":[false,true,true,true],"geometry_index":7,"in":0,"location":[24.763423,59.442671],"out":3,"turn_weight":5.0}],"maneuver":{"bearing_after":286,"bearing_before":189,"instruction":"Turn right onto Laeva.","location":[24.763423,59.442671],"modifier":"right","type":"turn"},"mode":"walking","name":"Laeva","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 26 feet, Bear right.","distanceAlongGeometry":8.0,"ssmlAnnouncement":"<speak>In 26 feet, Bear right.</speak>"}],"weight":17.424},{"bannerInstructions":[{"distanceAlongGeometry":15.0,"primary":{"components":[{"text":"Bear left onto the walkway.","type":"text"}],"modifier":"slight left","text":"Bear left onto the walkway.","type":"turn"}}],"distance":15.0,"driving_side":"right","duration":11.224,"geometry":"i`bkpBeplfn@oAvDkC|B","intersections":[{"admin_index":0,"bearings":[106,191,324],"entry":[false,true,true],"geometry_index":8,"in":0,"location":[24.763155,59.442709],"out":2,"turn_weight":5.0}],"maneuver":{"bearing_after":324,"bearing_before":286,"instruction":"Bear right.","location":[24.763155,59.442709],"modifier":"slight right","type":"turn"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 24 feet, Bear left onto the walkway.","distanceAlongGeometry":7.5,"ssmlAnnouncement":"<speak>In 24 feet, Bear left onto the walkway.</speak>"}],"weight":16.224},{"bannerInstructions":[{"distanceAlongGeometry":38.0,"primary":{"components":[{"text":"Continue.","type":"text"}],"modifier":"straight","text":"Continue.","type":"new name"}}],"distance":38.0,"driving_side":"right","duration":26.824,"geometry":"egbkpBoflfn@]~DMzAyCj^","intersections":[{"admin_index":0,"bearings":[1,70,145,287],"duration":5.647,"entry":[true,true,false,true],"geometry_index":10,"in":2,"location":[24.763,59.442819],"out":3,"weight":5.647},{"admin_index":0,"bearings":[107,158,287],"entry":[false,true,true],"geometry_index":12,"in":0,"location":[24.762858,59.442841],"out":2}],"maneuver":{"bearing_after":287,"bearing_before":325,"instruction":"Bear left onto the walkway.","location":[24.763,59.442819],"modifier":"slight left","type":"turn"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 62 feet, Continue.","distanceAlongGeometry":19.0,"ssmlAnnouncement":"<speak>In 62 feet, Continue.</speak>"}],"weight":26.824},{"bannerInstructions":[{"distanceAlongGeometry":7.0,"primary":{"components":[{"text":"Admiralisild; Admiral Bridge","type":"text"}],"modifier":"right","text":"Admiralisild; Admiral Bridge","type":"turn"}}],"distance":7.0,"driving_side":"right","duration":4.941,"geometry":"kmbkpBg~jfn@c@lF","intersections":[{"admin_index":0,"bearings":[107,155,287],"entry":[false,false,true],"geometry_index":13,"in":0,"location":[24.762356,59.442918],"out":2}],"maneuver":{"bearing_after":287,"bearing_before":287,"instruction":"Continue.","location":[24.762356,59.442918],"modifier":"straight","type":"new name"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 11 feet, Turn right onto Admiralisild/Admiral Bridge.","distanceAlongGeometry":3.5,"ssmlAnnouncement":"<speak>In 11 feet, Turn right onto Admiralisild/Admiral Bridge.</speak>"}],"weight":4.941},{"bannerInstructions":[{"distanceAlongGeometry":70.0,"primary":{"components":[{"text":"Continue on the walkway.","type":"text"}],"modifier":"straight","text":"Continue on the walkway.","type":"new name"}}],"distance":70.0,"driving_side":"right","duration":52.275,"geometry":"onbkpByvjfn@i@d@wIbHu@f@cV|P","intersections":[{"admin_index":0,"bearings":[107,168,336],"duration":16.235,"entry":[false,true,true],"geometry_index":14,"in":0,"location":[24.762237,59.442936],"out":2,"weight":16.235},{"admin_index":0,"bearings":[62,157,186,249,339],"duration":3.118,"entry":[true,false,true,true,true],"geometry_index":16,"in":1,"location":[24.762072,59.443129],"out":4,"turn_duration":1.0,"turn_weight":1.0,"weight":3.118},{"admin_index":0,"bearings":[159,338],"entry":[false,true],"geometry_index":17,"in":0,"location":[24.762052,59.443156],"out":1,"turn_weight":5.0}],"maneuver":{"bearing_after":336,"bearing_before":287,"instruction":"Turn right onto Admiralisild/Admiral Bridge.","location":[24.762237,59.442936],"modifier":"right","type":"turn"},"mode":"walking","name":"Admiralisild; Admiral Bridge","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 200 feet, Continue on the walkway.","distanceAlongGeometry":60.0,"ssmlAnnouncement":"<speak>In 200 feet, Continue on the walkway.</speak>"}],"weight":57.275},{"bannerInstructions":[{"distanceAlongGeometry":46.0,"primary":{"components":[{"text":"Turn left onto the walkway.","type":"text"}],"modifier":"left","text":"Turn left onto the walkway.","type":"turn"}}],"distance":46.0,"driving_side":"right","duration":33.471,"geometry":"ksckpBiyifn@kA~@_TxQ","intersections":[{"admin_index":0,"bearings":[158,337],"duration":3.529,"entry":[false,true],"geometry_index":18,"in":0,"location":[24.761765,59.443526],"out":1,"turn_weight":5.0,"weight":8.529},{"admin_index":0,"bearings":[70,157,246,336],"entry":[true,false,true,true],"geometry_index":19,"in":1,"location":[24.761733,59.443564],"out":3,"turn_duration":1.0,"turn_weight":1.0}],"maneuver":{"bearing_after":337,"bearing_before":338,"instruction":"Continue on the walkway.","location":[24.761765,59.443526],"modifier":"straight","type":"new name"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 75 feet, Turn left onto the walkway.","distanceAlongGeometry":23.0,"ssmlAnnouncement":"<speak>In 75 feet, Turn left onto the walkway.</speak>"}],"weight":38.471},{"bannerInstructions":[{"distanceAlongGeometry":131.0,"primary":{"components":[{"text":"Turn right onto the walkway.","type":"text"}],"modifier":"right","text":"Turn right onto the walkway.","type":"turn"}}],"distance":131.0,"driving_side":"right","duration":101.718,"geometry":"wjdkpBodifn@xX|eC","intersections":[{"admin_index":0,"bearings":[55,156,249,336],"entry":[true,false,true,true],"geometry_index":20,"in":1,"location":[24.761432,59.4439],"out":2}],"maneuver":{"bearing_after":249,"bearing_before":336,"instruction":"Turn left onto the walkway.","location":[24.761432,59.4439],"modifier":"left","type":"turn"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 200 feet, Turn right onto the walkway.","distanceAlongGeometry":60.0,"ssmlAnnouncement":"<speak>In 200 feet, Turn right onto the walkway.</speak>"}],"weight":101.718},{"bannerInstructions":[{"distanceAlongGeometry":25.0,"primary":{"components":[{"text":"Turn left onto the walkway.","type":"text"}],"modifier":"left","text":"Turn left onto the walkway.","type":"turn"}}],"distance":25.0,"driving_side":"right","duration":21.906,"geometry":"}pckpBq}dfn@{Az@qDrBw@b@{BnA","intersections":[{"admin_index":0,"bearings":[69,251,342],"duration":3.529,"entry":[false,true,true],"geometry_index":21,"in":0,"location":[24.759273,59.443487],"out":2,"weight":3.529},{"admin_index":0,"bearings":[70,162,258,342],"duration":9.471,"entry":[false,false,false,true],"geometry_index":22,"in":1,"location":[24.759243,59.443533],"out":3,"turn_duration":1.0,"turn_weight":1.0,"weight":10.318},{"admin_index":0,"bearings":[70,162,244,342],"entry":[true,false,true,true],"geometry_index":23,"in":1,"location":[24.759185,59.443622],"out":3,"turn_duration":1.0,"turn_weight":1.0}],"maneuver":{"bearing_after":342,"bearing_before":249,"instruction":"Turn right onto the walkway.","location":[24.759273,59.443487],"modifier":"right","type":"turn"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 41 feet, Turn left onto the walkway.","distanceAlongGeometry":12.5,"ssmlAnnouncement":"<speak>In 41 feet, Turn left onto the walkway.</speak>"}],"weight":23.148},{"bannerInstructions":[{"distanceAlongGeometry":16.0,"primary":{"components":[{"text":"Logi","type":"text"}],"modifier":"right","text":"Logi","type":"turn"}}],"distance":16.0,"driving_side":"right","duration":12.294,"geometry":"__dkpBmtdfn@TbCNjBd@rH","intersections":[{"admin_index":0,"bearings":[77,162,253,348],"duration":4.941,"entry":[true,false,true,true],"geometry_index":25,"in":1,"location":[24.759127,59.443712],"out":2,"weight":4.941},{"admin_index":0,"bearings":[73,168,256,335],"entry":[false,true,true,true],"geometry_index":27,"in":0,"location":[24.759007,59.443693],"out":2,"turn_duration":1.0,"turn_weight":1.0}],"maneuver":{"bearing_after":253,"bearing_before":342,"instruction":"Turn left onto the walkway.","location":[24.759127,59.443712],"modifier":"left","type":"turn"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 26 feet, Turn right onto Logi.","distanceAlongGeometry":8.0,"ssmlAnnouncement":"<speak>In 26 feet, Turn right onto Logi.</speak>"}],"weight":12.294},{"bannerInstructions":[{"distanceAlongGeometry":91.0,"primary":{"components":[{"text":"Turn left onto the walkway.","type":"text"}],"modifier":"left","text":"Turn left onto the walkway.","type":"turn"}}],"distance":91.0,"driving_side":"right","duration":72.235,"geometry":"s|ckpBicdfn@yAj@g@JiDrAcJxDcBjBcA^sDvAsIjDmCnD}@R","intersections":[{"admin_index":0,"bearings":[76,163,258,348],"duration":4.941,"entry":[false,true,true,true],"geometry_index":28,"in":0,"location":[24.758853,59.443674],"out":3,"weight":4.941},{"admin_index":0,"bearings":[61,168,248,345],"duration":28.118,"entry":[true,false,true,true],"geometry_index":30,"in":1,"location":[24.758825,59.443739],"out":3,"turn_duration":2.0,"turn_weight":7.0,"weight":33.118},{"admin_index":0,"bearings":[161,347],"duration":2.824,"entry":[false,true],"geometry_index":33,"in":0,"location":[24.758636,59.444052],"out":1,"weight":2.824},{"admin_index":0,"bearings":[82,167,258,346],"duration":8.059,"entry":[true,false,true,true],"geometry_index":34,"in":1,"location":[24.75862,59.444086],"out":3,"turn_duration":1.0,"turn_weight":1.0,"weight":8.059},{"admin_index":0,"bearings":[18,166,246,346],"duration":19.118,"entry":[true,false,true,true],"geometry_index":35,"in":1,"location":[24.758576,59.444176],"out":3,"turn_duration":5.0,"turn_weight":5.0,"weight":19.118},{"admin_index":0,"bearings":[166,253,328],"duration":6.353,"entry":[false,true,true],"geometry_index":36,"in":0,"location":[24.75849,59.444346],"out":2,"weight":6.353},{"admin_index":0,"bearings":[148,213,351],"entry":[false,true,true],"geometry_index":37,"in":0,"location":[24.758402,59.444417],"out":2}],"maneuver":{"bearing_after":348,"bearing_before":256,"instruction":"Turn right onto Logi.","location":[24.758853,59.443674],"modifier":"right","type":"turn"},"mode":"walking","name":"Logi","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 200 feet, Turn left onto the walkway.","distanceAlongGeometry":60.0,"ssmlAnnouncement":"<speak>In 200 feet, Turn left onto the walkway.</speak>"}],"weight":77.235},{"bannerInstructions":[{"distanceAlongGeometry":8.0,"primary":{"components":[{"text":"Turn right onto the walkway.","type":"text"}],"modifier":"right","text":"Turn right onto the walkway.","type":"turn"}}],"distance":8.0,"driving_side":"right","duration":5.647,"geometry":"_mekpBofcfn@`@bH","intersections":[{"admin_index":0,"bearings":[77,171,257,346],"entry":[true,false,true,true],"geometry_index":38,"in":1,"location":[24.758392,59.444448],"out":2,"turn_weight":5.0}],"maneuver":{"bearing_after":257,"bearing_before":351,"instruction":"Turn left onto the walkway.","location":[24.758392,59.444448],"modifier":"left","type":"turn"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 13 feet, Turn right onto the walkway.","distanceAlongGeometry":4.0,"ssmlAnnouncement":"<speak>In 13 feet, Turn right onto the walkway.</speak>"}],"weight":10.647},{"bannerInstructions":[{"distanceAlongGeometry":85.0,"primary":{"components":[{"text":"Kultuurikilomeeter","type":"text"}],"modifier":"slight left","text":"Kultuurikilomeeter","type":"turn"}}],"distance":85.0,"driving_side":"right","duration":64.447,"geometry":"}kekpBk}bfn@gHnBsRvGkDhCsDpT","intersections":[{"admin_index":0,"bearings":[77,213,349],"duration":48.918,"entry":[false,true,true],"geometry_index":39,"in":0,"location":[24.758246,59.444431],"out":2,"weight":48.918},{"admin_index":0,"bearings":[75,158,297],"entry":[true,false,true],"geometry_index":42,"in":1,"location":[24.757981,59.444979],"out":2}],"maneuver":{"bearing_after":349,"bearing_before":257,"instruction":"Turn right onto the walkway.","location":[24.758246,59.444431],"modifier":"right","type":"turn"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 200 feet, Bear left onto Kultuurikilomeeter.","distanceAlongGeometry":60.0,"ssmlAnnouncement":"<speak>In 200 feet, Bear left onto Kultuurikilomeeter.</speak>"}],"weight":64.447},{"bannerInstructions":[{"distanceAlongGeometry":1254.0,"primary":{"components":[{"text":"Turn right onto the walkway.","type":"text"}],"modifier":"right","text":"Turn right onto the walkway.","type":"turn"}}],"distance":1254.0,"driving_side":"right","duration":966.424,"geometry":"ysfkpBgwafn@pF~dEPfMfAft@H~FNrEdAt}@f@pY@rA?`@@rBJhRCdAIbD]nFa@bDaAbIiAdImB~MKt@wGrd@qBnOoDbUwAxJVfH\\jMHpEGzAiAjDqMbf@gBnFkC~HeDbKs@vBkCtF}CpGuIzNU`@oGzH{FhGqi@hc@ud@t_@wIpI{JfNqLfTwJjVgDdJ_HvYaEpUgHxa@aFhd@mErt@q@~FmFrd@oJdw@","intersections":[{"admin_index":0,"bearings":[117,266,329],"duration":127.059,"entry":[false,true,true],"geometry_index":43,"in":0,"location":[24.757636,59.445069],"out":1,"turn_weight":5.0,"weight":132.059},{"admin_index":0,"bearings":[86,175,266,355],"duration":13.205,"entry":[false,true,true,true],"geometry_index":44,"in":0,"location":[24.754468,59.444948],"out":2,"turn_duration":1.0,"turn_weight":1.0,"weight":15.035},{"admin_index":0,"bearings":[86,265],"duration":39.529,"entry":[false,true],"geometry_index":45,"in":0,"location":[24.75424,59.444939],"out":1,"weight":39.529},{"admin_index":0,"bearings":[86,262],"duration":4.235,"entry":[false,true],"geometry_index":47,"in":0,"location":[24.75326,59.444898],"out":1,"weight":4.235},{"admin_index":0,"bearings":[82,176,266],"duration":62.104,"entry":[false,true,true],"geometry_index":48,"in":0,"location":[24.753154,59.44489],"out":2,"weight":62.104},{"admin_index":0,"bearings":[86,176,268,358],"duration":3.824,"entry":[false,true,true,true],"geometry_index":51,"in":0,"location":[24.751684,59.444834],"out":2,"turn_duration":1.0,"turn_weight":6.0,"weight":8.824},{"admin_index":0,"bearings":[88,176,268,358],"duration":37.339,"entry":[false,true,true,true],"geometry_index":53,"in":0,"location":[24.751609,59.444833],"out":2,"turn_duration":1.0,"turn_weight":1.0,"weight":44.607},{"admin_index":0,"bearings":[109,134,292],"duration":15.529,"entry":[false,true,true],"geometry_index":58,"in":0,"location":[24.750981,59.444866],"out":2,"weight":15.529},{"admin_index":0,"bearings":[114,156,294],"duration":10.588,"entry":[false,true,true],"geometry_index":60,"in":0,"location":[24.750656,59.444936],"out":2,"weight":10.588},{"admin_index":0,"bearings":[114,169,294,359],"duration":39.824,"entry":[false,true,true,true],"geometry_index":61,"in":0,"location":[24.750416,59.444991],"out":2,"turn_duration":1.0,"turn_weight":1.0,"weight":39.824},{"admin_index":0,"bearings":[34,113,296],"duration":15.529,"entry":[true,false,true],"geometry_index":64,"in":1,"location":[24.749523,59.445194],"out":2,"weight":15.529},{"admin_index":0,"bearings":[116,295],"duration":14.118,"entry":[false,true],"geometry_index":65,"in":0,"location":[24.749169,59.445282],"out":1,"weight":14.118},{"admin_index":0,"bearings":[81,263,345],"duration":13.122,"entry":[false,true,true],"geometry_index":67,"in":0,"location":[24.748832,59.445314],"out":1,"weight":15.747},{"admin_index":0,"bearings":[83,183,268,351],"duration":8.353,"entry":[false,true,true,true],"geometry_index":68,"in":0,"location":[24.748602,59.445299],"out":2,"turn_duration":2.0,"turn_weight":7.0,"weight":13.353},{"admin_index":0,"bearings":[90,194,310],"duration":53.125,"entry":[false,true,true],"geometry_index":70,"in":0,"location":[24.748451,59.445298],"out":2,"weight":53.125},{"admin_index":0,"bearings":[131,222,310],"duration":21.699,"entry":[false,true,true],"geometry_index":74,"in":0,"location":[24.747459,59.44569],"out":2,"weight":21.699},{"admin_index":0,"bearings":[45,138,319],"duration":33.798,"entry":[true,false,true],"geometry_index":77,"in":1,"location":[24.747082,59.445869],"out":2,"weight":38.867},{"admin_index":0,"bearings":[56,143,237,328],"duration":100.953,"entry":[true,false,true,true],"geometry_index":79,"in":1,"location":[24.746691,59.446119],"out":3,"turn_duration":1.0,"turn_weight":1.0,"weight":105.951},{"admin_index":0,"bearings":[65,157,249,336],"duration":68.059,"entry":[true,false,true,true],"geometry_index":83,"in":1,"location":[24.745802,59.447073],"out":3,"turn_duration":1.0,"turn_weight":1.0,"weight":68.059},{"admin_index":0,"bearings":[61,153,244,327],"duration":80.059,"entry":[true,false,true,true],"geometry_index":85,"in":1,"location":[24.74511,59.447848],"out":3,"turn_duration":1.0,"turn_weight":1.0,"weight":84.012},{"admin_index":0,"bearings":[38,133,304],"duration":32.139,"entry":[true,false,true],"geometry_index":89,"in":1,"location":[24.743973,59.448527],"out":2,"weight":40.174},{"admin_index":0,"bearings":[124,215,298],"duration":102.353,"entry":[false,true,true],"geometry_index":90,"in":0,"location":[24.743545,59.448671],"out":2,"weight":102.353},{"admin_index":0,"bearings":[103,121,291],"duration":5.647,"entry":[false,true,true],"geometry_index":94,"in":0,"location":[24.741172,59.449132],"out":2,"weight":5.647},{"admin_index":0,"bearings":[20,111,291],"entry":[true,false,true],"geometry_index":95,"in":1,"location":[24.741044,59.449157],"out":2}],"maneuver":{"bearing_after":266,"bearing_before":297,"instruction":"Bear left onto Kultuurikilomeeter.","location":[24.757636,59.445069],"modifier":"slight left","type":"turn"},"mode":"walking","name":"Kultuurikilomeeter","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 200 feet, Turn right onto the walkway.","distanceAlongGeometry":60.0,"ssmlAnnouncement":"<speak>In 200 feet, Turn right onto the walkway.</speak>"}],"weight":1015.202},{"bannerInstructions":[{"distanceAlongGeometry":23.0,"primary":{"components":[{"text":"Turn left onto the walkway.","type":"text"}],"modifier":"left","text":"Turn left onto the walkway.","type":"turn"}}],"distance":23.0,"driving_side":"right","duration":18.235,"geometry":"gfokpBml~dn@kFmDsCyA","intersections":[{"admin_index":0,"bearings":[21,112,291],"duration":9.882,"entry":[true,false,true],"geometry_index":97,"in":1,"location":[24.739543,59.44946],"out":0,"turn_weight":5.0,"weight":14.882},{"admin_index":0,"bearings":[17,115,201,291],"entry":[true,true,false,true],"geometry_index":98,"in":2,"location":[24.73963,59.449578],"out":0,"turn_duration":2.0,"turn_weight":2.0}],"maneuver":{"bearing_after":21,"bearing_before":292,"instruction":"Turn right onto the walkway.","location":[24.739543,59.44946],"modifier":"right","type":"turn"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 37 feet, Turn left onto the walkway.","distanceAlongGeometry":11.5,"ssmlAnnouncement":"<speak>In 37 feet, Turn left onto the walkway.</speak>"}],"weight":23.235},{"bannerInstructions":[{"distanceAlongGeometry":16.0,"primary":{"components":[{"text":"Turn left onto the crosswalk.","type":"text"}],"modifier":"left","text":"Turn left onto the crosswalk.","type":"turn"}}],"distance":16.0,"driving_side":"right","duration":11.294,"geometry":"grokpBut~dn@yArJgAdA","intersections":[{"admin_index":0,"bearings":[111,197,304],"entry":[true,false,true],"geometry_index":99,"in":1,"location":[24.739675,59.449652],"out":2}],"maneuver":{"bearing_after":304,"bearing_before":17,"instruction":"Turn left onto the walkway.","location":[24.739675,59.449652],"modifier":"left","type":"turn"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 26 feet, Turn left onto the crosswalk.","distanceAlongGeometry":8.0,"ssmlAnnouncement":"<speak>In 26 feet, Turn left onto the crosswalk.</speak>"}],"weight":11.294},{"bannerInstructions":[{"distanceAlongGeometry":347.0,"primary":{"components":[{"text":"Turn right onto the walkway.","type":"text"}],"modifier":"right","text":"Turn right onto the walkway.","type":"turn"}}],"distance":347.0,"driving_side":"right","duration":263.849,"geometry":"iwokpB{f~dn@JhDm@`G_@fCMrAmAfFiKf|@{Fxh@oCdSi@dGaBrQcBbNwCd\\kGlh@uA~PuEzr@_@bHa@dC}@\\KbEOvC","intersections":[{"admin_index":0,"bearings":[8,127,262],"duration":3.529,"entry":[true,false,true],"geometry_index":101,"in":1,"location":[24.739454,59.449733],"out":2,"weight":3.529},{"admin_index":0,"bearings":[82,156,289,358],"duration":6.647,"entry":[false,true,true,true],"geometry_index":102,"in":0,"location":[24.739369,59.449727],"out":2,"turn_duration":1.0,"turn_weight":1.0,"weight":6.647},{"admin_index":0,"bearings":[44,109,255,295],"duration":3.823,"entry":[true,false,true,true],"geometry_index":103,"in":1,"location":[24.73924,59.44975],"out":3,"turn_duration":1.0,"turn_weight":1.0,"weight":3.823},{"admin_index":0,"bearings":[15,115,297],"duration":82.306,"entry":[true,false,true],"geometry_index":104,"in":1,"location":[24.739172,59.449766],"out":2,"weight":82.306},{"admin_index":0,"bearings":[110,201,294],"duration":15.529,"entry":[false,true,true],"geometry_index":108,"in":0,"location":[24.737365,59.450135],"out":2,"weight":15.529},{"admin_index":0,"bearings":[20,114,207,288],"duration":6.647,"entry":[true,false,true,true],"geometry_index":109,"in":1,"location":[24.737042,59.450207],"out":3,"turn_duration":1.0,"turn_weight":1.0,"weight":6.647},{"admin_index":0,"bearings":[32,108,288],"duration":42.353,"entry":[true,false,true],"geometry_index":110,"in":1,"location":[24.736911,59.450228],"out":2,"weight":42.353},{"admin_index":0,"bearings":[34,108,292],"duration":82.306,"entry":[true,false,true],"geometry_index":113,"in":1,"location":[24.735904,59.450403],"out":2,"weight":82.306},{"admin_index":0,"bearings":[104,191,295],"duration":12.649,"entry":[false,true,true],"geometry_index":116,"in":0,"location":[24.734123,59.450687],"out":2,"weight":13.282},{"admin_index":0,"bearings":[13,120,277],"duration":4.235,"entry":[true,false,true],"geometry_index":119,"in":1,"location":[24.733895,59.450751],"out":2,"weight":4.235},{"admin_index":0,"bearings":[14,97,193,282],"entry":[true,false,true,true],"geometry_index":120,"in":1,"location":[24.733797,59.450757],"out":3,"turn_duration":1.0,"turn_weight":1.0}],"maneuver":{"bearing_after":262,"bearing_before":307,"instruction":"Turn left onto the crosswalk.","location":[24.739454,59.449733],"modifier":"left","type":"turn"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 200 feet, Turn right onto the walkway.","distanceAlongGeometry":60.0,"ssmlAnnouncement":"<speak>In 200 feet, Turn right onto the walkway.</speak>"}],"weight":264.482},{"bannerInstructions":[{"distanceAlongGeometry":2.0,"primary":{"components":[{"text":"Turn left onto the walkway.","type":"text"}],"modifier":"left","text":"Turn left onto the walkway.","type":"turn"}}],"distance":2.0,"driving_side":"right","duration":1.412,"geometry":"ywqkpBq`sdn@k@F","intersections":[{"admin_index":0,"bearings":[102,269,355],"entry":[false,true,true],"geometry_index":121,"in":0,"location":[24.733721,59.450765],"out":2}],"maneuver":{"bearing_after":355,"bearing_before":282,"instruction":"Turn right onto the walkway.","location":[24.733721,59.450765],"modifier":"right","type":"turn"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 3 feet, Turn left onto the walkway.","distanceAlongGeometry":1.0,"ssmlAnnouncement":"<speak>In 3 feet, Turn left onto the walkway.</speak>"}],"weight":1.412},{"bannerInstructions":[{"distanceAlongGeometry":241.0,"primary":{"components":[{"text":"Allveelaeva","type":"text"}],"modifier":"slight left","text":"Allveelaeva","type":"turn"}}],"distance":241.0,"driving_side":"right","duration":184.456,"geometry":"eyqkpBi`sdn@oQbw@uNno@Gv@SxCo@hEiA`@i@nBf@pCQtDk@xC{B|KgTraAuA\\i@o@","intersections":[{"admin_index":0,"bearings":[14,175,303],"duration":45.642,"entry":[true,false,true],"geometry_index":122,"in":1,"location":[24.733717,59.450787],"out":2,"weight":45.642},{"admin_index":0,"bearings":[100,123,302],"duration":41.929,"entry":[true,false,true],"geometry_index":123,"in":1,"location":[24.732819,59.451083],"out":2,"weight":41.929},{"admin_index":0,"bearings":[119,213,284],"duration":2.823,"entry":[false,true,true],"geometry_index":125,"in":0,"location":[24.732015,59.451338],"out":2,"weight":2.823},{"admin_index":0,"bearings":[31,104,214,303],"duration":19.635,"entry":[true,false,true,true],"geometry_index":126,"in":1,"location":[24.731938,59.451348],"out":3,"turn_duration":1.0,"turn_weight":1.0,"weight":19.635},{"admin_index":0,"bearings":[25,89,193,299],"duration":4.529,"entry":[true,false,true,true],"geometry_index":131,"in":1,"location":[24.7316,59.451419],"out":3,"turn_duration":1.0,"turn_weight":1.0,"weight":4.529},{"admin_index":0,"bearings":[119,194,301],"duration":14.132,"entry":[false,true,true],"geometry_index":132,"in":0,"location":[24.731523,59.451441],"out":2,"weight":16.958},{"admin_index":0,"bearings":[38,121,302],"entry":[true,false,true],"geometry_index":133,"in":1,"location":[24.731316,59.451503],"out":2}],"maneuver":{"bearing_after":303,"bearing_before":355,"instruction":"Turn left onto the walkway.","location":[24.733717,59.450787],"modifier":"left","type":"turn"},"mode":"walking","name":"","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 200 feet, Bear left onto Allveelaeva.","distanceAlongGeometry":60.0,"ssmlAnnouncement":"<speak>In 200 feet, Bear left onto Allveelaeva.</speak>"}],"weight":187.283},{"bannerInstructions":[{"distanceAlongGeometry":28.0,"primary":{"components":[{"text":"Peetri","type":"text"}],"modifier":"right","text":"Peetri","type":"turn"}}],"distance":28.0,"driving_side":"right","duration":20.951,"geometry":"e_tkpBehldn@mFzY","intersections":[{"admin_index":0,"bearings":[32,120,152,299],"entry":[true,true,false,true],"geometry_index":136,"in":2,"location":[24.730259,59.451907],"out":3,"turn_weight":5.0}],"maneuver":{"bearing_after":299,"bearing_before":332,"instruction":"Bear left onto Allveelaeva.","location":[24.730259,59.451907],"modifier":"slight left","type":"turn"},"mode":"walking","name":"Allveelaeva","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 45 feet, Turn right onto Peetri.","distanceAlongGeometry":14.0,"ssmlAnnouncement":"<speak>In 45 feet, Turn right onto Peetri.</speak>"}],"weight":25.951},{"bannerInstructions":[{"distanceAlongGeometry":25.099,"primary":{"components":[{"text":"You have arrived at your destination.","type":"text"}],"text":"You have arrived at your destination.","type":"arrive"}}],"distance":25.099,"driving_side":"right","duration":24.804,"geometry":"sftkpBimkdn@}GiGqBoC","intersections":[{"admin_index":0,"bearings":[1,25,119,208],"entry":[true,true,false,true],"geometry_index":137,"in":2,"location":[24.729829,59.452026],"out":1,"turn_weight":5.0}],"maneuver":{"bearing_after":25,"bearing_before":299,"instruction":"Turn right onto Peetri.","location":[24.729829,59.452026],"modifier":"right","type":"turn"},"mode":"walking","name":"Peetri","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[{"announcement":"In 41 feet, You have arrived at your destination.","distanceAlongGeometry":12.5495,"ssmlAnnouncement":"<speak>In 41 feet, You have arrived at your destination.</speak>"}],"weight":54.607},{"bannerInstructions":[{"distanceAlongGeometry":0.0,"primary":{"components":[{"text":"You have arrived at your destination.","type":"text"}],"text":"You have arrived at your destination.","type":"arrive"}}],"distance":0.0,"driving_side":"right","duration":0.0,"geometry":"cstkpBczkdn@??","intersections":[{"admin_index":0,"bearings":[213],"entry":[true],"geometry_index":139,"in":0,"location":[24.730034,59.452226]}],"maneuver":{"bearing_after":0,"bearing_before":33,"instruction":"You have arrived at your destination.","location":[24.730034,59.452226],"type":"arrive"},"mode":"walking","name":"Peetri","speedLimitSign":"vienna","speedLimitUnit":"km/h","voiceInstructions":[],"weight":0.0}],"summary":"Logi, Kultuurikilomeeter","via_waypoints":[],"weight":2132.626}],"weight":2132.626,"weight_name":"pedestrian"}],"waypoints":[{"distance":0.546,"location":[24.765368,59.442643],"name":""},{"distance":0.134,"location":[24.730034,59.452226],"name":"Peetri"}]}"#;
    const VALHALLA_OSRM_RESPONSE_VIA_WAYS: &str = r#"{"code":"Ok","routes":[{"distance":2089.442,"duration":301.262,"geometry":"oop|u@ntan{Cn@gArJ{HvKuCpVsHxC_AhEiBpCkA|AN|Bn@fC}@dI_IjGmHfH_EpBwBxI{PfIsOzGgMpGyQnDmIvDmHf@gCT{AdByTtByKVoCJwBBqA`@oPe@uQe@eNMaTK{Lu@aO}EoWsDgL{KaTiDwHoC{K_AkAu@_BcC{DsHgKuDoGcE_KwB}JsAqPIeJ`@}IvAeKd@mEd@wCDmBGkLAoGv@qHKaGYqFw@kI{@}NToI~@yIjBaJtAkF`D{JxGkQ~J}O`KuJtSmOjKgLtJ}IhB}EvBcIfAiJ`CgJb@gHp@iLnAmQzBcNnCkLhGaUpFmMzHiMdKqMhIoLjJcQjEgKzAgFtAqGvAgNjC}NbAkGTeMp@aG`CiKzCwJdI}QvKcOfG{MtEmPdBsP","legs":[{"admins":[{"iso_3166_1":"US","iso_3166_1_alpha3":"USA"}],"annotation":{"distance":[4.4,25.8,23.9,44.6,9.1,12.4,8.9,5.3,7.4,8.2,23.9,21.0,19.0,8.6,33.9,31.7,27.3,33.1,19.0,17.9,7.0,4.7,34.5,21.0,7.2,5.9,4.0,27.4,29.2,23.8,32.9,21.7,25.3,40.2,23.0,40.1,17.9,21.6,5.1,5.6,11.8,25.7,16.7,21.7,19.8,27.8,17.5,17.2,19.6,10.3,7.7,5.4,20.9,13.3,15.2,12.6,11.9,16.5,25.1,16.4,17.2,18.3,12.5,20.6,32.7,34.0,28.2,44.9,30.2,26.9,12.3,17.2,18.1,19.0,14.6,21.0,29.1,24.6,22.4,37.5,26.3,28.4,31.4,28.0,34.8,22.2,12.4,14.2,24.3,26.1,13.6,22.2,12.9,20.5,20.3,34.7,33.9,27.5,29.7,28.1],"duration":[0.635,3.717,3.436,6.419,1.314,1.782,1.286,0.762,1.065,1.174,3.447,3.019,2.729,1.243,4.882,4.569,3.929,4.765,2.738,2.584,1.008,0.67,4.971,3.031,1.03,0.848,0.577,3.943,4.211,3.427,4.736,3.12,3.636,5.787,3.309,5.772,2.581,3.116,0.74,0.801,1.692,3.698,2.404,3.121,2.85,4.005,2.516,2.473,2.829,1.479,1.11,0.774,3.007,1.911,2.196,1.815,1.713,2.375,3.614,2.367,2.484,2.633,1.795,2.968,4.708,4.896,4.059,6.466,4.352,3.877,1.776,2.471,2.607,2.735,2.099,3.019,4.194,3.542,3.22,5.398,3.781,4.094,4.526,4.025,5.011,3.202,1.789,2.044,3.5,3.754,1.96,3.194,1.856,2.957,2.922,4.995,4.882,3.957,4.278,4.048],"maxspeed":[{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true},{"unknown":true}],"speed":[6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9,6.9]},"distance":2089.442,"duration":301.262,"steps":[{"bannerInstructions":[{"distanceAlongGeometry":2089.442,"primary":{"components":[{"text":"You have arrived at your destination.","type":"text"}],"text":"You have arrived at your destination.","type":"arrive"}}],"distance":2089.442,"driving_side":"right","duration":301.262,"geometry":"oop|u@ntan{Cn@gArJ{HvKuCpVsHxC_AhEiBpCkA|AN|Bn@fC}@dI_IjGmHfH_EpBwBxI{PfIsOzGgMpGyQnDmIvDmHf@gCT{AdByTtByKVoCJwBBqA`@oPe@uQe@eNMaTK{Lu@aO}EoWsDgL{KaTiDwHoC{K_AkAu@_BcC{DsHgKuDoGcE_KwB}JsAqPIeJ`@}IvAeKd@mEd@wCDmBGkLAoGv@qHKaGYqFw@kI{@}NToI~@yIjBaJtAkF`D{JxGkQ~J}O`KuJtSmOjKgLtJ}IhB}EvBcIfAiJ`CgJb@gHp@iLnAmQzBcNnCkLhGaUpFmMzHiMdKqMhIoLjJcQjEgKzAgFtAqGvAgNjC}NbAkGTeMp@aG`CiKzCwJdI}QvKcOfG{MtEmPdBsP","intersections":[{"admin_index":0,"bearings":[134],"duration":4.352,"entry":[true],"geometry_index":0,"location":[-82.036056,28.795656],"out":0,"weight":8.922},{"admin_index":0,"bearings":[160,162,323],"duration":9.823,"entry":[false,true,false],"geometry_index":2,"in":2,"location":[-82.035862,28.795446],"out":1,"weight":20.138},{"admin_index":0,"bearings":[160,340],"duration":3.024,"entry":[true,false],"geometry_index":4,"in":1,"location":[-82.035633,28.794865],"out":0,"turn_weight":1.5,"weight":7.699},{"admin_index":0,"bearings":[155,335],"duration":4.32,"entry":[true,false],"geometry_index":6,"in":1,"location":[-82.035548,28.794687],"out":0,"turn_weight":1.5,"weight":10.356},{"admin_index":0,"bearings":[59,139,239,338],"duration":10.445,"entry":[true,true,true,false],"geometry_index":10,"in":3,"location":[-82.035511,28.794436],"out":1,"turn_duration":0.077,"turn_weight":6.0,"weight":27.254},{"admin_index":0,"bearings":[52,125,239,317],"duration":24.525,"entry":[true,true,true,false],"geometry_index":14,"in":3,"location":[-82.035044,28.793934],"out":1,"turn_duration":0.045,"turn_weight":6.0,"weight":56.184},{"admin_index":0,"bearings":[18,103,199,290],"duration":10.542,"entry":[true,true,true,false],"geometry_index":21,"in":3,"location":[-82.033577,28.793118],"out":1,"turn_duration":0.03,"turn_weight":6.0,"weight":27.55},{"admin_index":0,"bearings":[7,94,184,277],"duration":45.814,"entry":[true,true,true,false],"geometry_index":26,"in":3,"location":[-82.032845,28.792979],"out":1,"turn_duration":0.022,"turn_weight":6.0,"weight":99.874},{"admin_index":0,"bearings":[51,148,234,324],"duration":28.246,"entry":[true,true,false,true],"geometry_index":40,"in":2,"location":[-82.029777,28.793661],"out":0,"turn_duration":0.022,"turn_weight":6.0,"weight":63.859},{"admin_index":0,"bearings":[8,92,186,286],"duration":9.7,"entry":[true,true,true,false],"geometry_index":51,"in":3,"location":[-82.027959,28.794078],"out":1,"turn_duration":0.052,"turn_weight":6.6,"weight":26.378},{"admin_index":0,"bearings":[83,182,267,356],"duration":17.016,"entry":[true,true,false,true],"geometry_index":56,"in":2,"location":[-82.027272,28.794058],"out":0,"turn_duration":0.024,"turn_weight":6.6,"weight":41.434},{"admin_index":0,"bearings":[116,293],"duration":33.12,"entry":[true,false],"geometry_index":63,"in":1,"location":[-82.026094,28.793989],"out":0,"turn_weight":1.65,"weight":69.546},{"admin_index":0,"bearings":[30,113,208,299],"duration":9.962,"entry":[true,true,true,false],"geometry_index":71,"in":3,"location":[-82.024391,28.792613],"out":1,"turn_duration":0.026,"turn_weight":6.6,"weight":26.969},{"admin_index":0,"bearings":[13,98,191,278],"duration":10.808,"entry":[true,true,true,false],"geometry_index":75,"in":3,"location":[-82.02372,28.792434],"out":1,"turn_duration":0.008,"turn_weight":6.6,"weight":28.74},{"admin_index":0,"bearings":[18,111,199,286],"duration":35.002,"entry":[true,true,true,false],"geometry_index":78,"in":3,"location":[-82.02297,28.792307],"out":1,"turn_duration":0.01,"turn_weight":6.6,"weight":78.334},{"admin_index":0,"bearings":[25,110,191,294],"duration":9.384,"entry":[true,true,true,false],"geometry_index":87,"in":3,"location":[-82.020892,28.791133],"out":1,"turn_duration":0.024,"turn_weight":8.8,"weight":27.988},{"admin_index":0,"bearings":[106,204,287],"duration":5.203,"entry":[true,false,false],"geometry_index":90,"in":2,"location":[-82.020256,28.790976],"out":0,"turn_duration":0.019,"turn_weight":1.65,"weight":12.277},{"admin_index":0,"bearings":[7,102,184,273],"duration":7.788,"entry":[true,true,true,false],"geometry_index":92,"in":3,"location":[-82.019895,28.790931],"out":1,"turn_duration":0.012,"turn_weight":6.6,"weight":22.541},{"admin_index":0,"bearings":[122,213,295],"entry":[true,false,false],"geometry_index":95,"in":2,"location":[-82.019381,28.790763],"out":0,"turn_duration":0.01,"turn_weight":1.65}],"maneuver":{"bearing_after":134,"bearing_before":0,"instruction":"Drive southeast.","location":[-82.036056,28.795656],"type":"depart"},"mode":"driving","name":"","speedLimitSign":"mutcd","speedLimitUnit":"mph","voiceInstructions":[{"announcement":"Drive southeast.","distanceAlongGeometry":2089.442,"ssmlAnnouncement":"<speak>Drive southeast.</speak>"},{"announcement":"In 200 feet, You have arrived at your destination.","distanceAlongGeometry":60.0,"ssmlAnnouncement":"<speak>In 200 feet, You have arrived at your destination.</speak>"}],"weight":703.153},{"bannerInstructions":[{"distanceAlongGeometry":0.0,"primary":{"components":[{"text":"You have arrived at your destination.","type":"text"}],"text":"You have arrived at your destination.","type":"arrive"}}],"distance":0.0,"driving_side":"right","duration":0.0,"geometry":"ste|u@hm~l{C??","intersections":[{"admin_index":0,"bearings":[282],"entry":[true],"geometry_index":100,"in":0,"location":[-82.018021,28.790106]}],"maneuver":{"bearing_after":0,"bearing_before":102,"instruction":"You have arrived at your destination.","location":[-82.018021,28.790106],"type":"arrive"},"mode":"driving","name":"","speedLimitSign":"mutcd","speedLimitUnit":"mph","voiceInstructions":[],"weight":0.0}],"summary":"","via_waypoints":[{"distance_from_start":30.223,"geometry_index":2,"waypoint_index":1}],"weight":703.153}],"weight":703.153,"weight_name":"golf_cart"}],"waypoints":[{"distance":0.095,"location":[-82.036056,28.795656],"name":""},{"distance":0.0,"location":[-82.035862,28.795446],"name":""},{"distance":4.898,"location":[-82.018021,28.790106],"name":""}]}"#;
    const VALHALLA_EXTENDED_OSRM_RESPONSE: &str = r#"{"routes":[{"weight_name":"auto","weight":462.665,"duration":182.357,"distance":1718.205,"legs":[{"via_waypoints":[],"admins":[{"iso_3166_1_alpha3":"USA","iso_3166_1":"US"}],"weight":462.665,"duration":182.357,"steps":[{"bannerInstructions":[{"primary":{"type":"end of road","modifier":"right","text":"John F. Kennedy Boulevard","components":[{"text":"John F. Kennedy Boulevard","type":"text"},{"text":"/","type":"delimiter"},{"text":"CR 501","type":"text"}]},"distanceAlongGeometry":64.13}],"intersections":[{"classes":["restricted"],"entry":[true],"bearings":[151],"duration":16.247,"admin_index":0,"out":0,"weight":18.684,"geometry_index":0,"location":[-74.031614,40.775707]},{"entry":[false,true,false,false],"classes":["restricted"],"in":3,"bearings":[121,175,239,331],"duration":3.995,"turn_weight":15,"turn_duration":0.035,"admin_index":0,"out":1,"weight":19.554,"geometry_index":1,"location":[-74.031354,40.775349]},{"bearings":[63,159,244,355],"entry":[false,true,false,false],"classes":["restricted"],"in":3,"turn_weight":15,"turn_duration":0.061,"admin_index":0,"out":1,"geometry_index":2,"location":[-74.031343,40.775254]}],"maneuver":{"instruction":"Drive southeast.","type":"depart","bearing_after":151,"bearing_before":0,"location":[-74.031614,40.775707]},"name":"","duration":23.182,"distance":64.13,"driving_side":"right","weight":56.55,"mode":"driving","geometry":"u`wwlAz~oelCjUgO|DU|B_A"},{"bannerInstructions":[{"primary":{"type":"on ramp","modifier":"slight left","text":"Take the ramp on the left.","components":[{"text":"Take the ramp on the left.","type":"text"}]},"distanceAlongGeometry":115}],"intersections":[{"entry":[false,true,false],"in":2,"bearings":[63,252,339],"duration":5.392,"turn_weight":20,"turn_duration":2.423,"admin_index":0,"out":1,"weight":23.414,"geometry_index":3,"location":[-74.031311,40.775191]},{"entry":[false,true,true,true],"in":0,"bearings":[99,144,282,328],"duration":4.598,"turn_weight":10,"lanes":[{"indications":["left"],"valid":false,"active":false},{"indications":["straight"],"valid_indication":"straight","valid":true,"active":true},{"indications":["straight"],"valid_indication":"straight","valid":true,"active":false}],"turn_duration":2.008,"admin_index":0,"out":2,"weight":12.978,"geometry_index":9,"location":[-74.031856,40.775165]},{"bearings":[48,94,269],"entry":[false,false,true],"in":1,"turn_weight":2.5,"turn_duration":0.026,"admin_index":0,"out":2,"geometry_index":12,"location":[-74.032336,40.775218]}],"maneuver":{"modifier":"right","instruction":"Turn right onto John F. Kennedy Boulevard/CR 501.","type":"end of road","bearing_after":252,"bearing_before":159,"location":[-74.031311,40.775191]},"name":"John F. Kennedy Boulevard","duration":12.446,"distance":115,"driving_side":"right","weight":41.686,"mode":"driving","ref":"CR 501","geometry":"m`vwlA|koelCl@fCj@lC\\zEUfHQzC[jCgApJ[dJEfFFjS"},{"bannerInstructions":[{"primary":{"type":"fork","modifier":"slight right","text":"NJ 495 West, NJTP West","components":[{"text":"NJ 495 West, NJTP West","type":"text"},{"text":"/","type":"delimiter"},{"text":"NJ 495","type":"text"}]},"distanceAlongGeometry":236}],"intersections":[{"entry":[false,true,true],"in":0,"bearings":[89,249,265],"duration":17.813,"turn_duration":0.083,"admin_index":0,"out":1,"weight":20.39,"geometry_index":13,"location":[-74.032662,40.775214]},{"bearings":[37,237],"entry":[false,true],"in":0,"admin_index":0,"out":1,"geometry_index":26,"location":[-74.034357,40.77406]}],"maneuver":{"modifier":"slight left","instruction":"Take the ramp on the left.","type":"on ramp","bearing_after":249,"bearing_before":269,"location":[-74.032662,40.775214]},"name":"","duration":21.323,"distance":236,"driving_side":"right","weight":24.514,"mode":"driving","geometry":"{avwlAj`relCrBbJVvBXvBh@pCh@dCh@tBj@lBxB~FrBdErCrEhC`DjCrCzg@|g@v@fAdAlBn@fC\\fBPfEJzE"},{"intersections":[{"entry":[false,false,true,true],"classes":["motorway"],"in":0,"bearings":[82,120,260,300],"duration":10.631,"turn_weight":2.1,"turn_duration":0.088,"admin_index":0,"out":3,"weight":14.488,"geometry_index":32,"location":[-74.034778,40.773943]},{"entry":[false,false,true],"classes":["motorway"],"in":1,"bearings":[108,114,290],"duration":0.924,"turn_duration":0.024,"admin_index":0,"out":2,"weight":1.057,"geometry_index":43,"location":[-74.037391,40.774928]},{"entry":[true,false,true],"classes":["motorway"],"in":1,"bearings":[27,110,289],"duration":4.905,"turn_duration":0.019,"admin_index":0,"out":2,"weight":5.741,"geometry_index":44,"location":[-74.037621,40.774991]},{"entry":[false,false,true],"classes":["motorway"],"in":0,"bearings":[100,114,295],"duration":0.65,"turn_weight":30.2,"turn_duration":0.02,"admin_index":0,"out":2,"weight":30.94,"geometry_index":47,"location":[-74.03891,40.775288]},{"entry":[false,true,true],"classes":["motorway"],"in":0,"bearings":[115,296,318],"duration":1.087,"lanes":[{"indications":["straight"],"valid_indication":"straight","valid":true,"active":false},{"indications":["straight"],"valid_indication":"straight","valid":true,"active":false},{"indications":["straight"],"valid_indication":"straight","valid":true,"active":false},{"indications":["straight","slight right"],"valid_indication":"straight","valid":true,"active":true}],"turn_duration":0.007,"admin_index":0,"out":1,"weight":1.269,"geometry_index":48,"location":[-74.039057,40.775341]},{"bearings":[116,296],"entry":[false,true],"classes":["motorway"],"in":0,"admin_index":0,"out":1,"geometry_index":49,"location":[-74.039315,40.775435]}],"bannerInstructions":[{"secondary":{"text":"US 1 South, US 9 South: Jersey City","components":[{"text":"US 1 South, US 9 South: Jersey City","type":"text"}]},"primary":{"type":"off ramp","modifier":"slight right","text":"Tonnelle Avenue","components":[{"text":"Tonnelle Avenue","type":"text"},{"text":"/","type":"delimiter"},{"text":"US 1; US 9","type":"text"}]},"distanceAlongGeometry":558},{"distanceAlongGeometry":400,"primary":{"type":"off ramp","modifier":"slight right","text":"Tonnelle Avenue","components":[{"text":"Tonnelle Avenue","type":"text"},{"text":"/","type":"delimiter"},{"text":"US 1; US 9","type":"text"}]},"secondary":{"text":"US 1 South, US 9 South: Jersey City","components":[{"text":"US 1 South, US 9 South: Jersey City","type":"text"}]},"sub":{"text":"","components":[{"active":false,"text":"","directions":["straight"],"type":"lane"},{"active":false,"text":"","directions":["straight"],"type":"lane"},{"active":false,"text":"","directions":["straight"],"type":"lane"},{"active_direction":"right","active":true,"text":"","directions":["straight","right"],"type":"lane"}]}}],"destinations":"NJ 495 West, NJTP West","maneuver":{"modifier":"slight right","instruction":"Keep right to take NJ 495 West/NJTP West.","type":"fork","bearing_after":300,"bearing_before":262,"location":[-74.034778,40.773943]},"name":"","duration":24.452,"distance":558,"driving_side":"right","weight":60.845,"mode":"driving","ref":"NJ 495","geometry":"mrswlArdvelCqNnb@{CbJoBpGqBvGmB`HyBjIwBpImBvH}AzGwE~S}Hv\\}BjMqKpn@}AtIaBhUiBdH{DbOo`@t{A"},{"intersections":[{"entry":[false,true,true],"in":0,"bearings":[116,296,313],"duration":25.673,"lanes":[{"indications":["straight"],"valid":false,"active":false},{"indications":["straight"],"valid":false,"active":false},{"indications":["straight"],"valid":false,"active":false},{"indications":["straight","right"],"valid_indication":"right","valid":true,"active":true}],"turn_duration":0.023,"admin_index":0,"out":2,"weight":30.139,"geometry_index":50,"location":[-74.040798,40.775971]},{"entry":[true,false],"in":1,"bearings":[172,323],"duration":10.463,"admin_index":0,"out":0,"weight":12.293,"geometry_index":90,"location":[-74.040181,40.776747]},{"bearings":[18,30,207],"entry":[false,true,true],"in":0,"turn_weight":27.4,"turn_duration":0.013,"admin_index":0,"out":2,"geometry_index":101,"location":[-74.040403,40.775953]}],"bannerInstructions":[{"primary":{"type":"turn","modifier":"slight right","text":"29th Street","components":[{"text":"29th Street","type":"text"}]},"distanceAlongGeometry":372}],"destinations":"US 1 South, US 9 South: Jersey City","maneuver":{"modifier":"slight right","instruction":"Take the US 1 South/US 9 South exit toward Jersey City.","type":"off ramp","bearing_after":313,"bearing_before":296,"location":[-74.040798,40.775971]},"name":"Tonnelle Avenue","duration":38.663,"distance":372,"driving_side":"right","weight":72.787,"mode":"driving","ref":"US 1; US 9","geometry":"eqwwlAz|aflC_LnQiBvCwAlBuArAeAv@mAp@sAh@sA^kATgAJeAAyAIgAIeAWkAc@mAo@cAs@kAgAcAgAy@{A{@cBm@wAk@gBc@sBY{BOsBGuBA_CBcBP{BZwB\\uAl@wBr@aBr@sA|@wAz@gA~@aAlA_AlAs@z@YlAY`ASz@K|@CdADjAN~@VpAj@lDfBzWpIrXbP"},{"bannerInstructions":[{"primary":{"type":"new name","modifier":"right","text":"Dell Avenue","components":[{"text":"Dell Avenue","type":"text"}]},"distanceAlongGeometry":84}],"intersections":[{"entry":[false,true,true],"in":0,"bearings":[27,207,249],"duration":5.755,"turn_weight":11.3,"turn_duration":0.115,"admin_index":0,"out":2,"weight":17.927,"geometry_index":102,"location":[-74.040677,40.775543]},{"entry":[false,true,true],"in":0,"bearings":[106,128,293],"duration":0.371,"turn_weight":4.2,"turn_duration":0.011,"admin_index":0,"out":2,"weight":4.623,"geometry_index":108,"location":[-74.041213,40.775524]},{"bearings":[113,201,297],"entry":[false,true,true],"in":0,"turn_weight":4.2,"turn_duration":0.009,"admin_index":0,"out":2,"geometry_index":109,"location":[-74.04125,40.775536]}],"maneuver":{"modifier":"slight right","instruction":"Bear right onto 29th Street.","type":"turn","bearing_after":249,"bearing_before":207,"location":[-74.040677,40.775543]},"name":"29th Street","duration":10.215,"distance":84,"driving_side":"right","weight":31.544,"mode":"driving","geometry":"mvvwlAhuaflCvAfHXjBLnB?`C[nC}@xGWhAqGjU"},{"bannerInstructions":[{"primary":{"type":"arrive","modifier":"left","text":"Your destination is on the left.","components":[{"text":"Your destination is on the left.","type":"text"}]},"distanceAlongGeometry":289.074}],"intersections":[{"entry":[true,false],"in":1,"bearings":[27,117],"duration":4.14,"turn_weight":88.4,"admin_index":0,"out":0,"weight":93.264,"geometry_index":110,"location":[-74.041608,40.775673]},{"entry":[true,true,false],"in":2,"bearings":[27,115,207],"duration":13.507,"turn_weight":4.2,"turn_duration":0.007,"admin_index":0,"out":0,"weight":20.062,"geometry_index":111,"location":[-74.041485,40.775855]},{"entry":[true,true,false],"in":2,"bearings":[27,115,207],"duration":0.547,"turn_weight":4.2,"turn_duration":0.007,"admin_index":0,"out":0,"weight":4.835,"geometry_index":112,"location":[-74.041079,40.776457]},{"entry":[true,false,true],"in":1,"bearings":[27,207,303],"duration":13.687,"turn_weight":4.2,"turn_duration":0.007,"admin_index":0,"out":0,"weight":20.274,"geometry_index":113,"location":[-74.041062,40.776482]},{"entry":[true,false,true],"in":1,"bearings":[27,207,296],"duration":12.607,"turn_weight":4.2,"turn_duration":0.007,"admin_index":0,"out":0,"weight":19.005,"geometry_index":114,"location":[-74.040653,40.777088]},{"entry":[true,true,false],"in":2,"bearings":[27,115,207],"duration":5.767,"turn_weight":4.2,"turn_duration":0.007,"admin_index":0,"out":0,"weight":10.968,"geometry_index":115,"location":[-74.040274,40.777651]},{"bearings":[27,115,207],"entry":[true,true,false],"in":2,"turn_weight":4.2,"turn_duration":0.007,"admin_index":0,"out":0,"geometry_index":116,"location":[-74.040103,40.777904]}],"maneuver":{"modifier":"right","instruction":"Turn right onto Dell Avenue.","type":"new name","bearing_after":27,"bearing_before":297,"location":[-74.041608,40.775673]},"name":"Dell Avenue","duration":52.075,"distance":289.074,"driving_side":"right","weight":174.739,"mode":"driving","geometry":"q~vwlAnocflCkJuFsd@kXq@a@{d@qXeb@uVyNuIaDmB"},{"intersections":[{"bearings":[207],"entry":[true],"in":0,"admin_index":0,"geometry_index":117,"location":[-74.040048,40.777985]}],"bannerInstructions":[],"maneuver":{"modifier":"left","instruction":"Your destination is on the left.","type":"arrive","bearing_after":0,"bearing_before":27,"location":[-74.040048,40.777985]},"name":"Dell Avenue","duration":0,"distance":0,"driving_side":"right","weight":0,"mode":"driving","geometry":"ao{wlA~m`flC??"}],"distance":1718.205,"summary":"NJ 495, US 1"}],"geometry":"u`wwlAz~oelCjUgO|DU|B_Al@fCj@lC\\zEUfHQzC[jCgApJ[dJEfFFjSrBbJVvBXvBh@pCh@dCh@tBj@lBxB~FrBdErCrEhC`DjCrCzg@|g@v@fAdAlBn@fC\\fBPfEJzEqNnb@{CbJoBpGqBvGmB`HyBjIwBpImBvH}AzGwE~S}Hv\\}BjMqKpn@}AtIaBhUiBdH{DbOo`@t{A_LnQiBvCwAlBuArAeAv@mAp@sAh@sA^kATgAJeAAyAIgAIeAWkAc@mAo@cAs@kAgAcAgAy@{A{@cBm@wAk@gBc@sBY{BOsBGuBA_CBcBP{BZwB\\uAl@wBr@aBr@sA|@wAz@gA~@aAlA_AlAs@z@YlAY`ASz@K|@CdADjAN~@VpAj@lDfBzWpIrXbPvAfHXjBLnB?`C[nC}@xGWhAqGjUkJuFsd@kXq@a@{d@qXeb@uVyNuIaDmB"}],"waypoints":[{"distance":0.446,"name":"","location":[-74.031614,40.775707]},{"distance":20.629,"name":"Dell Avenue","location":[-74.040048,40.777985]}],"code":"Ok"}"#;

    #[test]
    fn parse_standard_osrm() {
        let parser = OsrmResponseParser::new(6);
        let routes = parser
            .parse_response(STANDARD_OSRM_POLYLINE6_RESPONSE.into())
            .expect("Unable to parse OSRM response");
        insta::assert_yaml_snapshot!(routes);
    }

    #[test]
    fn parse_valhalla_osrm() {
        let parser = OsrmResponseParser::new(6);
        let routes = parser
            .parse_response(VALHALLA_OSRM_RESPONSE.into())
            .expect("Unable to parse Valhalla OSRM response");

        insta::assert_yaml_snapshot!(routes, {
            ".**.annotations" => "redacted annotations json strings vec"
        });
    }

    #[test]
    fn parse_valhalla_osrm_with_via_ways() {
        let parser = OsrmResponseParser::new(6);
        let routes = parser
            .parse_response(VALHALLA_OSRM_RESPONSE_VIA_WAYS.into())
            .expect("Unable to parse Valhalla OSRM response");

        insta::assert_yaml_snapshot!(routes, {
            ".**.annotations" => "redacted annotations json strings vec"
        });
    }

    #[test]
    fn parse_valhalla_asserting_annotation_lengths() {
        let parser = OsrmResponseParser::new(6);
        let routes = parser
            .parse_response(VALHALLA_OSRM_RESPONSE.into())
            .expect("Unable to parse Valhalla OSRM response");

        // Loop through every step and validate that the length of the annotations
        // matches the length of the geometry minus one. This is because each annotation
        // represents a segment between two coordinates.
        for (route_index, route) in routes.iter().enumerate() {
            for (step_index, step) in route.steps.iter().enumerate() {
                if step_index == route.steps.len() - 1 {
                    // The arrival step is 2 of the same coordinates.
                    // So annotations will be None.
                    assert_eq!(step.annotations, None);
                    continue;
                }

                let step = step.clone();
                let annotations = step.annotations.expect("No annotations");
                assert_eq!(
                    annotations.len(),
                    step.geometry.len() - 1,
                    "Route {}, Step {}",
                    route_index,
                    step_index
                );
            }
        }
    }

    #[test]
    fn parse_valhalla_asserting_sub_maneuvers() {
        let parser = OsrmResponseParser::new(6);
        let routes = parser
            .parse_response(VALHALLA_EXTENDED_OSRM_RESPONSE.into())
            .expect("Unable to parse Valhalla Extended OSRM response");

        // Collect all sub_contents into a vector
        let sub_contents: Vec<_> = routes
            .iter()
            .flat_map(|route| &route.steps)
            .filter_map(|step| {
                step.visual_instructions
                    .iter()
                    .find_map(|instruction: &VisualInstruction| instruction.sub_content.as_ref())
            })
            .collect();

        // Assert that there's exactly one sub maneuver instructions as is the case in the test data
        assert_eq!(
            sub_contents.len(),
            1,
            "Expected exactly one sub banner instructions"
        );

        if let Some(sub_content) = sub_contents.first() {
            // Ensure that there are 4 pieces of lane information in the sub banner instructions
            if let Some(lane_info) = &sub_content.lane_info {
                assert_eq!(lane_info.len(), 4);
            } else {
                panic!("Expected lane information, but could not find it");
            }
        } else {
            panic!("No sub banner instructions found in any of the steps")
        }
    }
}
